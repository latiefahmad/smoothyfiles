//! Plays a clip in a window of its own (flip-model swap chain on the decoder's device) for N
//! seconds, then closes it. Reports pictures shown and dropped, the A/V offset, and what the back
//! buffer held at a few points (read back just before the present: PrintWindow does not see
//! flip-model content).
//!
//! `cargo run --release -p rustdecoder --example play_window -- <clip> [seconds] [--size WxH] [--audible] [--save dir]`
//!
//! Muted unless `--audible` is given (session volume 0, session mute, silent buffers). The
//! window is shown without being activated and is closed by the example.

#[cfg(windows)]
fn png(path: &std::path::Path, c: &rustdecoder::window::Capture) {
    use std::io::Write;
    let mut raw = Vec::with_capacity((c.width * c.height * 4 + c.height) as usize);
    for y in 0..c.height {
        raw.push(0);
        for x in 0..c.width {
            let p = c.pixel(x, y);
            raw.extend_from_slice(&[p[0], p[1], p[2], 255]);
        }
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    z.write_all(&raw).unwrap();
    let data = z.finish().unwrap();
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut chunk = |kind: &[u8; 4], body: &[u8]| {
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        let mut c = crc32fast::Hasher::new();
        c.update(kind);
        c.update(body);
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out.extend_from_slice(&c.finalize().to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&c.width.to_be_bytes());
    ihdr.extend_from_slice(&c.height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &data);
    chunk(b"IEND", &[]);
    std::fs::write(path, out).unwrap();
}

#[cfg(windows)]
fn main() {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use rustdecoder::window::{Placement, VideoWindow, WindowOptions};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: play_window <clip> [seconds] [--size WxH] [--audible] [--save dir]");
        std::process::exit(2);
    };
    let audible = args.iter().any(|a| a == "--audible");
    let seconds: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5.0);
    let (w, h) = args
        .iter()
        .position(|a| a == "--size")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
        .unwrap_or((800, 600));
    let save = args.iter().position(|a| a == "--save").and_then(|i| args.get(i + 1)).map(std::path::PathBuf::from);
    if let Some(d) = &save {
        let _ = std::fs::create_dir_all(d);
    }

    let bg = [0.10, 0.12, 0.35, 1.0];
    let t0 = Instant::now();
    let win = match VideoWindow::open(
        Path::new(path),
        Placement::TopLevel { title: "rustdecoder play_window".into(), width: w, height: h },
        WindowOptions { background: bg, audio: true, muted: !audible, volume: 1.0, ..Default::default() },
    ) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("open: {e}");
            std::process::exit(1);
        }
    };
    let st = win.state();
    println!("opened in {:.0} ms: {} {}x{} picture in a {}x{} window; {}", t0.elapsed().as_secs_f64() * 1000.0, st.codec, st.video_size.0, st.video_size.1, st.size.0, st.size.1, if audible { "AUDIBLE" } else { "muted" });
    println!("audio: {}", if st.has_audio { "yes".to_string() } else { format!("no ({})", st.audio_note) });

    let describe = |name: &str, c: &rustdecoder::window::Capture| {
        let (cx, cy) = (c.width / 2, c.height / 2);
        let centre = c.mean(cx.saturating_sub(8), cy.saturating_sub(8), 16, 16);
        let corner = c.pixel(2, 2);
        println!("  capture {name}: {}x{}, corner rgb {:?}, centre mean rgb [{:.0} {:.0} {:.0}]", c.width, c.height, &corner[..3], centre[0], centre[1], centre[2]);
        if let Some(d) = &save {
            png(&d.join(format!("{name}.png")), c);
        }
    };

    let start = Instant::now();
    let mut samples: Vec<(f64, f64)> = Vec::new();
    let mut next_sample = 0.0;
    let mut captured = [false; 3];
    while start.elapsed().as_secs_f64() < seconds {
        let el = start.elapsed().as_secs_f64();
        let s = win.state();
        if el >= next_sample {
            next_sample += 0.25;
            if let (Some(p), Some(a)) = (s.picture, s.audio_position) {
                samples.push((el, p - a));
            }
        }
        for (i, frac) in [0.25, 0.5, 0.75].into_iter().enumerate() {
            if !captured[i] && el >= seconds * frac {
                captured[i] = true;
                match win.capture() {
                    Ok(c) => describe(&format!("at-{}pct", (frac * 100.0) as u32), &c),
                    Err(e) => println!("  capture failed: {e}"),
                }
            }
        }
        if s.ended || s.error.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let s = win.state();
    println!("played {:.2} s: position {:.2} / {:.2} s, presented {}, shown {}, dropped {}, late {}", start.elapsed().as_secs_f64(), s.position, s.duration, s.presented, s.shown, s.dropped, s.late);
    if !samples.is_empty() {
        let n = samples.len() as f64;
        let mean = samples.iter().map(|s| s.1).sum::<f64>() / n;
        let (lo, hi) = samples.iter().fold((f64::MAX, f64::MIN), |(l, h), s| (l.min(s.1), h.max(s.1)));
        println!("A/V offset (picture pts - sound heard): mean {:+.2} ms, min {:+.2}, max {:+.2} over {} samples", mean * 1000.0, lo * 1000.0, hi * 1000.0, samples.len());
    }
    if let Some(e) = &s.error {
        println!("stopped with: {e}");
    }

    // pause: nothing may be presented
    win.pause();
    std::thread::sleep(Duration::from_millis(150));
    let before = win.state().presented;
    std::thread::sleep(Duration::from_millis(500));
    let after = win.state().presented;
    println!("paused for 0.5 s: {} presents", after - before);

    // seek while paused: one new picture
    win.seek(s.duration * 0.5);
    std::thread::sleep(Duration::from_millis(400));
    if let Ok(c) = win.capture() {
        describe("after-seek", &c);
    }
    let st = win.state();
    println!("after seek: position {:.2}, picture {:?}, presented {}", st.position, st.picture, st.presented);

    // resize the window: the swap chain follows
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::*;
        let _ = SetWindowPos(HWND(win.hwnd() as *mut _), None, 0, 0, 500, 500, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
    std::thread::sleep(Duration::from_millis(300));
    if let Ok(c) = win.capture() {
        describe("after-resize", &c);
    }
    println!("closing");
    drop(win);
}

#[cfg(not(windows))]
fn main() {}
