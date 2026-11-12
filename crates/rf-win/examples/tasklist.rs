//! Prints the windows a taskbar would show, grouped like taskbar buttons (read-only).
//!
//!   cargo run --release -p rf-win --example tasklist

fn main() {
    let start = std::time::Instant::now();
    let tasks = rf_win::tasklist::tasks();
    let took = start.elapsed();
    let n = tasks.len();
    for (key, windows) in rf_win::tasklist::grouped(tasks) {
        println!("{key}");
        for w in windows {
            println!("    {}{}", if w.minimized { "(küçük) " } else { "" }, w.title);
        }
    }
    println!("\n{n} pencere, {took:?}");
}
