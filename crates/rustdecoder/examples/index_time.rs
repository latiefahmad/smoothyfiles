//! Times how long the container index takes: `index_time <file>` prints the tracks and the time
//! `container::read` needed, then decodes sample 0 of each track to show the read path works.

use std::fs::File;
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: index_time <file>");
    let size = std::fs::metadata(&path).unwrap().len();
    let mut f = File::open(&path).unwrap();
    let t = Instant::now();
    let movie = rustdecoder::container::read(&mut f).unwrap();
    let dt = t.elapsed();
    println!("{path}: {:.2} GB indexed in {:.2} s ({:.0} MB/s), {:.1} s long", size as f64 / 1e9, dt.as_secs_f64(), size as f64 / 1e6 / dt.as_secs_f64(), movie.duration_secs());
    let mut buf = Vec::new();
    for tr in &movie.tracks {
        println!("  track {} {:?} {} samples, {}x{}, {} Hz {} ch, codec {:?}", tr.id, tr.kind, tr.samples.len(), tr.width, tr.height, tr.sample_rate, tr.channels, std::mem::discriminant(&tr.codec));
        if !tr.samples.is_empty() {
            let mid = tr.samples.len() / 2;
            let t = Instant::now();
            tr.read_sample(&mut f, mid, &mut buf).unwrap();
            println!("    sample {mid} ({} bytes) read in {:.2} ms", buf.len(), t.elapsed().as_secs_f64() * 1000.0);
        }
    }
}
