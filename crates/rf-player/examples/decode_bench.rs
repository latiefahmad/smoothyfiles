//! Çözme hızı ölçümü: `cargo run --release -p rf-player --example decode_bench -- <dosya>`

use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("dosya yolu");
    let t = Instant::now();
    let m = rf_player::open(std::path::Path::new(&path))?;
    let opened = t.elapsed();
    let v = m.info().video.clone().expect("video");
    let fps = v.frame_rate.as_f64();
    let n = (m.duration() * fps).floor() as usize;
    let t = Instant::now();
    for i in 0..n {
        m.frame_at(i as f64 / fps)?;
    }
    let el = t.elapsed();
    println!("{}x{} {} kare: açma {:?}, çözme {:.1} kare/s", v.width, v.height, n, opened, n as f64 / el.as_secs_f64());
    let f = m.frame_at(0.0)?;
    for (w, h) in [(1600u32, 900u32), (v.width, v.height)] {
        let t = Instant::now();
        for _ in 0..20 {
            std::hint::black_box(rf_player::to_bgra(&f, w, h));
        }
        println!("  BGRA {w}x{h}: {:.2} ms/kare", t.elapsed().as_secs_f64() * 1000.0 / 20.0);
    }
    Ok(())
}
