//! WASAPI çıkışını elle denemek: `cargo run --release -p rf-player --example wasapi_check -- <ses dosyası>`
//! (sessiz bir dosyayla çalıştırın).

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("dosya");
    let sink = rf_player::wasapi_sink()?;
    println!("aygıt: {} Hz, {} kanal", sink.sample_rate(), sink.channels());
    let mut p = rf_player::Player::new(rf_player::open(std::path::Path::new(&path))?, sink);
    p.play();
    for _ in 0..6 {
        std::thread::sleep(std::time::Duration::from_millis(250));
        println!("{:?}", p.state());
    }
    Ok(())
}
