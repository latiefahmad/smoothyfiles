//! GPU decode speed: `cargo run --release -p rustdecoder --example decode_speed -- <clip> [frames] [-v]`
//! Decodes without any read-back and waits for the GPU at the end. `-v` lists every frame.

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verbose = args.iter().any(|a| a == "-v");
    let args: Vec<&String> = args.iter().filter(|a| *a != "-v").collect();
    let Some(path) = args.first() else {
        eprintln!("usage: decode_speed <clip> [frames] [-v]");
        std::process::exit(2);
    };
    let max: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let mut dec = rustdecoder::VideoDecoder::open(std::path::Path::new(path)).expect("open");
    let info = dec.info().clone();
    println!("{} {}x{} {}-bit, {} samples, {:.2} fps, adapter {}", dec.codec_name(), info.width, info.height, info.bit_depth, info.frame_count, info.frame_rate, dec.adapter());
    let t0 = std::time::Instant::now();
    let mut n = 0usize;
    while n < max {
        let Some(f) = dec.next_frame().expect("decode") else { break };
        if verbose {
            println!("frame {n}: POC {} pts {} surface {}", f.poc, f.pts, f.index);
        }
        n += 1;
    }
    dec.wait_idle().expect("wait");
    let s = t0.elapsed().as_secs_f64();
    println!("{n} frames in {s:.3} s: {:.1} fps (GPU decode, no read-back; ConfigBitstreamRaw {:?}, short slice format)", n as f64 / s, dec.config_bitstream_raw());
}

#[cfg(not(windows))]
fn main() {}
