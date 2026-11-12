//! Plays a clip headless at real-time speed: decode ahead, pick the frame for the clock, convert
//! it to RGBA on the GPU (no window). Reports frames shown, dropped, late and the GPU time of
//! one conversion.
//!
//! `cargo run --release -p rustdecoder --example play_headless -- <clip> [seconds] [out_w out_h] [--from secs]`
//!
//! The output box defaults to 1920x1080 (swapped for a clip stored rotated, so the pixel count
//! stays the same); the picture is rotated for display and fitted into it.

#[cfg(windows)]
fn main() {
    use rustdecoder::player::Player;
    use rustdecoder::present::{Colorimetry, Converter, Params, fit, rotated_size};
    use std::time::{Duration, Instant};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: play_headless <clip> [seconds] [out_w out_h] [--from secs]");
        std::process::exit(2);
    };
    let from: f64 = args.iter().position(|a| a == "--from").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let nums: Vec<f64> = args.iter().skip(1).take_while(|a| *a != "--from").filter_map(|s| s.parse().ok()).collect();
    let seconds = nums.first().copied().unwrap_or(5.0);

    let mut pl = Player::open(std::path::Path::new(path), 6).expect("open");
    let info = pl.info().clone();
    let (vw, vh) = rotated_size(info.width, info.height, info.rotation);
    let (bw, bh) = match (nums.get(1), nums.get(2)) {
        (Some(w), Some(h)) => (*w as u32, *h as u32),
        _ if vh > vw => (1080, 1920),
        _ => (1920, 1080),
    };
    let (ow, oh) = fit(vw, vh, bw, bh);
    println!("{}x{} {}-bit coded, rotation {}, {:.2} fps, shown {vw}x{vh} -> output {ow}x{oh}", info.width, info.height, info.bit_depth, info.rotation, info.frame_rate);
    let mut conv = Converter::new(pl.device(), pl.context()).expect("converter");
    let colour = Colorimetry::from_vui(&info.vui, info.width, info.height);
    println!("colour: {colour:?}");
    let params = Params { colour, rotation: info.rotation, flip: false };
    let target = conv.create_target(ow, oh).expect("target");

    let (start, end) = pl.range();
    let begin = (start + from).min(end);
    if from > 0.0 {
        pl.seek(begin);
    }
    // let the first pictures arrive so the run measures steady playback, not start-up
    pl.wait_ahead(begin, Duration::from_secs(5));
    let t0 = Instant::now();
    pl.clock.start(begin);
    let mut converts = 0u64;
    let mut cpu_convert = Duration::ZERO;
    let mut last_serial = 0;
    let mut polls = 0u64;
    let mut worst_gap = 0.0f64;
    let mut last_new = Instant::now();
    while t0.elapsed().as_secs_f64() < seconds && !pl.finished() {
        polls += 1;
        if let Some(f) = pl.frame_now()
            && f.serial != last_serial
        {
            last_serial = f.serial;
            let t = Instant::now();
            conv.convert(&f.source(), &params, &target).expect("convert");
            cpu_convert += t.elapsed();
            converts += 1;
            worst_gap = worst_gap.max(last_new.elapsed().as_secs_f64());
            last_new = Instant::now();
        }
        std::thread::sleep(Duration::from_micros(500));
    }
    let wall = t0.elapsed().as_secs_f64();
    let st = pl.stats();
    // let the timestamp queries come back
    for _ in 0..10 {
        let _ = conv.read_rgba(&target);
        if conv.gpu_time().1 as u64 >= converts.saturating_sub(4) {
            break;
        }
    }
    let (gpu_ms, n) = conv.gpu_time();
    println!("played {wall:.2} s of movie time from {begin:.2} s: {converts} frames converted ({:.1} fps), {polls} polls", converts as f64 / wall);
    println!("shown {}, dropped {}, late {}, decoded {} (ring {}, most slots in use {})", st.shown, st.dropped, st.late, st.decoded, st.ring, st.max_in_flight);
    println!("longest wait between two new frames {:.1} ms (frame time {:.1} ms)", worst_gap * 1000.0, 1000.0 / info.frame_rate.max(1.0));
    println!("convert: GPU {gpu_ms:.3} ms per frame ({n} measured), CPU to record the commands {:.3} ms", cpu_convert.as_secs_f64() * 1000.0 / converts.max(1) as f64);
    if let Some(e) = pl.error() {
        println!("decoder stopped: {e}");
    }
}

#[cfg(not(windows))]
fn main() {}
