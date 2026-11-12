//! Büyük metin dosyasının açılış süresi: `cargo run --release -p rf-core --example textload_bench -- <dosya>`
fn main() {
    let p = std::env::args().nth(1).expect("dosya");
    let t = std::time::Instant::now();
    let f = rf_core::textload::TextFile::open(std::path::Path::new(&p)).unwrap();
    f.index_chunk();
    let first = f.line(0).unwrap();
    let ready = t.elapsed();
    f.index_all();
    println!("ilk satır {:?} sonra hazır ({} bayt), tüm dizin {:?}, {} satır, {}", ready, first.len(), t.elapsed(), f.line_count(), f.encoding_name());
}
