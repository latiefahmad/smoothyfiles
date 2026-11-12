//! The indexer service following changes through the change journal: index a temp folder,
//! change things in it, and time until searches see the changes.
//! `cargo run --release -p rf-win --example indexer_follow`

use std::path::Path;
use std::time::{Duration, Instant};

use rf_win::indexer;

fn wait_for(what: &str, root: &Path, pattern: &str, want: bool) {
    let t = Instant::now();
    loop {
        let hits = indexer::search(pattern, root, false, 100).unwrap_or_default();
        if hits.is_empty() != want {
            println!("{what}: seen after {:.1} s", t.elapsed().as_secs_f32());
            return;
        }
        if t.elapsed() > Duration::from_secs(15) {
            println!("{what}: NOT seen after 15 s");
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn main() {
    let base = std::env::temp_dir().join(format!("rf-follow-{}", std::process::id()));
    let root = base.join("root");
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    std::fs::write(root.join("a/b/old.txt"), b"x").unwrap();
    indexer::start(vec![root.clone()], base.join("index"));
    let t = Instant::now();
    while !indexer::status().iter().all(|s| s.1 == indexer::State::Ready) {
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("ready after {:.1} s", t.elapsed().as_secs_f32());
    std::thread::sleep(Duration::from_millis(1500));
    std::fs::write(root.join("a/b/zebra note.txt"), b"x").unwrap();
    wait_for("new file deep inside", &root, "zebra", true);
    std::fs::create_dir_all(root.join("new/inner")).unwrap();
    std::fs::write(root.join("new/inner/quokka.md"), b"x").unwrap();
    wait_for("new folder with a file", &root, "quokka", true);
    std::fs::remove_file(root.join("a/b/old.txt")).unwrap();
    wait_for("deleted file", &root, "old.txt", false);
    std::fs::rename(root.join("a"), root.join("renamed")).unwrap();
    let t = Instant::now();
    loop {
        let hits = indexer::search("zebra", &root, false, 10).unwrap_or_default();
        if hits.iter().any(|p| p.starts_with(root.join("renamed"))) {
            println!("renamed folder: seen after {:.1} s", t.elapsed().as_secs_f32());
            break;
        }
        if t.elapsed() > Duration::from_secs(15) {
            println!("renamed folder: NOT seen after 15 s ({hits:?})");
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    indexer::stop();
    let _ = std::fs::remove_dir_all(&base);
}
