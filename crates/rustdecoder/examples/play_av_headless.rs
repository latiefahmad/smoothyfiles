//! Plays a clip with its sound headless at real-time speed, the audio clock as master: decode
//! ahead, pick the picture for the audio clock, convert it on the GPU (no window). Reports the
//! A/V drift over time (the picture shown against the sound heard) and dropped pictures.
//!
//! `cargo run --release -p rustdecoder --example play_av_headless -- <clip> [seconds] [--from secs] [--audible]`
//!
//! Muted unless `--audible` is given: the session volume is set to 0, the session muted, and
//! the buffers handed to the device are silent, so the run makes no sound.

#[cfg(windows)]
fn main() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use rustdecoder::audio_out::{AudioOutput, AudioSink, OutputOptions};
    use rustdecoder::playback::Playback;
    use rustdecoder::present::{Colorimetry, Converter, Params, fit, rotated_size};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: play_av_headless <clip> [seconds] [--from secs] [--audible]");
        std::process::exit(2);
    };
    let audible = args.iter().any(|a| a == "--audible");
    let from: f64 = args.iter().position(|a| a == "--from").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let seconds: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5.0);

    let out = match AudioOutput::open(OutputOptions { muted: !audible, volume: 1.0 }) {
        Ok(o) => Some(Arc::new(o)),
        Err(e) => {
            println!("no audio output ({e}): video only, system clock");
            None
        }
    };
    let sink: Option<Arc<dyn AudioSink>> = out.clone().map(|o| o as Arc<dyn AudioSink>);
    let mut pb = Playback::open_with_sink(std::path::Path::new(path), 6, sink).expect("open");
    let info = pb.player.info().clone();
    match pb.audio_info() {
        Some(a) => {
            let (dr, dc) = out.as_ref().unwrap().device_format();
            println!(
                "audio: {} {} Hz {} ch, {:.2} s; device {dr} Hz {dc} ch{}; {}",
                a.codec,
                a.sample_rate,
                a.channels,
                a.duration,
                if a.sample_rate != dr { " (resampled)" } else { "" },
                if audible { "AUDIBLE" } else { "muted" }
            );
        }
        None => println!("no audio ({}), system clock", pb.audio_note()),
    }
    let (vw, vh) = rotated_size(info.width, info.height, info.rotation);
    let (bw, bh) = if vh > vw { (1080, 1920) } else { (1920, 1080) };
    let (ow, oh) = fit(vw, vh, bw, bh);
    println!("video: {}x{} {}-bit, rotation {}, {:.2} fps -> {ow}x{oh}", info.width, info.height, info.bit_depth, info.rotation, info.frame_rate);
    let mut conv = Converter::new(pb.player.device(), pb.player.context()).expect("converter");
    let params = Params { colour: Colorimetry::from_vui(&info.vui, info.width, info.height), rotation: info.rotation, flip: false };
    let target = conv.create_target(ow, oh).expect("target");

    let (start, end) = pb.player.range();
    let begin = (start + from).min(end);
    if from > 0.0 {
        pb.seek(begin);
    }
    let t0 = Instant::now();
    pb.play();
    println!("started in {:.0} ms (preroll)", t0.elapsed().as_secs_f64() * 1000.0);
    let t0 = Instant::now();
    let clock0 = pb.time();
    let mut last_serial = 0;
    let mut last_pts = f64::NAN;
    let mut converts = 0u64;
    // drift samples: (wall s, picture pts - audio position, picture pts - clock, clock - audio)
    let mut samples: Vec<(f64, f64, f64, f64)> = Vec::new();
    let mut next_sample = 0.0;
    let mut worst_gap = 0.0f64;
    let mut last_new = Instant::now();
    while t0.elapsed().as_secs_f64() < seconds && !pb.finished() {
        if let Some(f) = pb.frame()
            && f.serial != last_serial
        {
            last_serial = f.serial;
            last_pts = f.pts;
            conv.convert(&f.source(), &params, &target).expect("convert");
            converts += 1;
            worst_gap = worst_gap.max(last_new.elapsed().as_secs_f64());
            last_new = Instant::now();
        }
        let wall = t0.elapsed().as_secs_f64();
        if wall >= next_sample && last_pts.is_finite() {
            next_sample += 0.25;
            let clock = pb.time();
            let audio = pb.audio_position().unwrap_or(clock);
            samples.push((wall, last_pts - audio, last_pts - clock, clock - audio));
        }
        std::thread::sleep(Duration::from_micros(500));
    }
    let wall = t0.elapsed().as_secs_f64();
    let clock_adv = pb.time() - clock0;
    let st = pb.stats();
    println!("played {wall:.2} s wall, movie clock advanced {clock_adv:.3} s, {converts} pictures converted ({:.1} fps)", converts as f64 / wall);
    println!("shown {}, dropped {}, late {}, decoded {} (ring {}, most slots in use {})", st.shown, st.dropped, st.late, st.decoded, st.ring, st.max_in_flight);
    println!("longest wait between two new pictures {:.1} ms (frame time {:.1} ms)", worst_gap * 1000.0, 1000.0 / info.frame_rate.max(1.0));
    if pb.has_audio() {
        println!("A/V drift (picture pts - sound heard, ms), every 0.25 s:");
        for (t, d, dc, ca) in &samples {
            println!("  {t:5.2} s  {:+7.2}   (picture - clock {:+6.2}, clock - sound {:+6.3})", d * 1000.0, dc * 1000.0, ca * 1000.0);
        }
        let n = samples.len().max(1) as f64;
        let mean = samples.iter().map(|s| s.1).sum::<f64>() / n;
        let (lo, hi) = samples.iter().fold((f64::MAX, f64::MIN), |(l, h), s| (l.min(s.1), h.max(s.1)));
        // the slope of the drift over the run: does the picture creep away from the sound?
        let slope = if samples.len() > 2 {
            let (sx, sy): (f64, f64) = samples.iter().fold((0.0, 0.0), |a, s| (a.0 + s.0, a.1 + s.1));
            let (mx, my) = (sx / n, sy / n);
            let num: f64 = samples.iter().map(|s| (s.0 - mx) * (s.1 - my)).sum();
            let den: f64 = samples.iter().map(|s| (s.0 - mx).powi(2)).sum();
            num / den
        } else {
            0.0
        };
        println!("drift: mean {:+.2} ms, min {:+.2}, max {:+.2}, trend {:+.3} ms per second (one frame is {:.1} ms)", mean * 1000.0, lo * 1000.0, hi * 1000.0, slope * 1000.0, 1000.0 / info.frame_rate.max(1.0));
        if let Some(o) = &out {
            println!("audio under-runs {}, device periods {}", o.underruns(), o.device_periods());
        }
    }
    if let Some(e) = pb.player.error() {
        println!("decoder stopped: {e}");
    }
}

#[cfg(not(windows))]
fn main() {}
