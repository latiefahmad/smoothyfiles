//! MPEG-1 / MPEG-2 video decoded on the GPU compared with ffmpeg's software decoder, in program
//! streams, transport streams, MXF and raw elementary streams. The MPEG-2 IDCT on a GPU is not
//! the same one ffmpeg uses, so the pictures are not bit-exact: the test measures the error
//! (largest difference, how many samples differ) and holds it to a small bound. Clips are made
//! with ffmpeg into a temp folder; tests skip with a message when ffmpeg or a GPU decoder is
//! missing.

#![cfg(windows)]

mod common;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::{have_ffmpeg, temp_dir};
use rustdecoder::VideoDecoder;

struct Clip<'a> {
    name: &'a str,
    size: &'a str,
    secs: u32,
    /// Encoder arguments after `-c:v`.
    args: &'a [&'a str],
    /// Muxer arguments (format) before the output name.
    mux: &'a [&'a str],
}

fn make_clip(c: &Clip) -> PathBuf {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let out = temp_dir().join(c.name);
    if out.exists() {
        return out;
    }
    let tmp = out.with_file_name(format!("part-{}", c.name));
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size={}:rate=25", c.size))
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", &c.secs.to_string(), "-vf", "noise=alls=5:allf=t", "-pix_fmt", "yuv420p", "-c:v"])
        .args(c.args)
        .args(["-c:a", "mp2", "-b:a", "128k", "-ac", "2"])
        .args(c.mux)
        .arg(&tmp)
        .status()
        .expect("ffmpeg");
    assert!(status.success(), "ffmpeg could not make {}", c.name);
    std::fs::rename(&tmp, &out).unwrap();
    out
}

#[derive(Debug)]
struct Report {
    frames: usize,
    ref_frames: usize,
    max_diff: u32,
    /// Samples (luma and chroma) that differ, of all compared.
    differing: u64,
    total: u64,
    /// Histogram of |difference| 0, 1, 2, 3.., capped at 8.
    hist: [u64; 9],
    pts_monotonic: bool,
    codec: &'static str,
}

fn compare(clip: &Path) -> Option<Report> {
    let mut dec = match VideoDecoder::open(clip) {
        Ok(d) => d,
        Err(rustdecoder::Error::Gpu(m)) => {
            eprintln!("skipped: {m}");
            return None;
        }
        Err(e) => panic!("open {}: {e}", clip.display()),
    };
    let info = dec.info().clone();
    let mut ff = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(clip)
        .args(["-map", "0:v:0", "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "nv12", "-"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("ffmpeg");
    let mut out = ff.stdout.take().unwrap();
    let mut r = Report { frames: 0, ref_frames: 0, max_diff: 0, differing: 0, total: 0, hist: [0; 9], pts_monotonic: true, codec: dec.codec_name() };
    let (w, h) = (info.width as usize, info.height as usize);
    let mut ref_frame = vec![0u8; w * h * 3 / 2 + w * (h % 2)];
    let mut last_pts = i64::MIN;
    while let Some(f) = dec.next_frame().unwrap() {
        let planes = dec.read_back(&f).unwrap();
        assert_eq!((planes.width as usize, planes.height as usize), (w, h));
        r.pts_monotonic &= f.pts > last_pts;
        last_pts = f.pts;
        let cw = w.div_ceil(2);
        let ch = h.div_ceil(2);
        ref_frame.resize(w * h + cw * 2 * ch, 0);
        if out.read_exact(&mut ref_frame).is_err() {
            break;
        }
        if std::env::var("MPEG2_FRAMES").is_ok() {
            if r.frames == 0 {
                let _ = std::fs::write(temp_dir().join("m2dump.y"), planes.y.iter().map(|v| *v as u8).collect::<Vec<u8>>());
            }
            let sad: u64 = planes.y.iter().zip(&ref_frame[..w * h]).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64).sum();
            if r.frames == 0 { let bands: Vec<String> = (0..h / 16).map(|b| { let s: u64 = (b * 16 * w..(b + 1) * 16 * w).map(|i| (planes.y[i] as i32 - ref_frame[i] as i32).unsigned_abs() as u64).sum(); format!("{:.0}", s as f64 / (16 * w) as f64) }).collect(); eprintln!("bands {}", bands.join(" ")); }
            eprintln!("  y[..6]={:?} ref={:?}", &planes.y[..6], &ref_frame[..6]);
            eprintln!("frame {} pts {}: mean |diff| luma {:.2}", r.frames, f.pts, sad as f64 / (w * h) as f64);
        }
        r.frames += 1;
        let (ry, ruv) = ref_frame.split_at(w * h);
        for (a, b) in planes.y.iter().zip(ry).chain(planes.uv.iter().zip(ruv)) {
            let d = (*a as i32 - *b as i32).unsigned_abs();
            r.total += 1;
            r.hist[d.min(8) as usize] += 1;
            if d != 0 {
                r.differing += 1;
            }
            r.max_diff = r.max_diff.max(d);
        }
    }
    r.ref_frames = r.frames;
    let mut extra = Vec::new();
    r.ref_frames += out.read_to_end(&mut extra).map_or(0, |n| n / ref_frame.len().max(1));
    let _ = ff.kill();
    let _ = ff.wait();
    Some(r)
}

fn check(c: Clip, frames: usize, max_diff: u32) {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let clip = make_clip(&c);
    let Some(r) = compare(&clip) else { return };
    eprintln!(
        "{}: {} ({}) {} frames, ffmpeg {}; max |diff| {}, {:.4}% of samples differ; histogram of |diff| 0..8+: {:?}; pts increasing: {}",
        c.name,
        r.codec,
        c.args.join(" "),
        r.frames,
        r.ref_frames,
        r.max_diff,
        100.0 * r.differing as f64 / r.total.max(1) as f64,
        r.hist,
        r.pts_monotonic
    );
    assert_eq!(r.frames, frames, "{}: frame count", c.name);
    assert_eq!(r.ref_frames, frames, "{}: ffmpeg frame count", c.name);
    assert!(r.max_diff <= max_diff, "{}: largest difference {} (allowed {max_diff})", c.name, r.max_diff);
    let within1 = (r.hist[0] + r.hist[1]) as f64 / r.total.max(1) as f64;
    assert!(within1 >= 0.98, "{}: only {:.2}% of samples within 1", c.name, 100.0 * within1);
    assert!(r.pts_monotonic, "{}: presentation times not increasing", c.name);
}


#[test]
fn mpeg2_progressive_ps_no_b_frames() {
    check(Clip { name: "m2_prog_nob.mpg", size: "640x352", secs: 3, args: &["mpeg2video", "-b:v", "4M", "-g", "12", "-bf", "0"], mux: &["-f", "mpeg"] }, 75, 12);
}

#[test]
fn mpeg2_progressive_ps_b_frames() {
    check(Clip { name: "m2_prog_b.mpg", size: "640x352", secs: 3, args: &["mpeg2video", "-b:v", "4M", "-g", "12", "-bf", "2"], mux: &["-f", "mpeg"] }, 75, 12);
}

#[test]
fn mpeg2_interlaced_ps_b_frames() {
    check(Clip { name: "m2_int_b.mpg", size: "720x576", secs: 2, args: &["mpeg2video", "-b:v", "6M", "-g", "12", "-bf", "2", "-flags", "+ilme+ildct", "-top", "1"], mux: &["-f", "vob"] }, 50, 12);
}

#[test]
fn mpeg2_custom_matrices_ts() {
    let intra: Vec<String> = (0..64).map(|i| (8 + i / 2).to_string()).collect();
    let inter: Vec<String> = (0..64).map(|i| (16 + i / 4).to_string()).collect();
    let intra = intra.join(",");
    let inter = inter.join(",");
    let args = ["mpeg2video", "-b:v", "4M", "-g", "12", "-bf", "2", "-intra_matrix", &intra, "-inter_matrix", &inter];
    check(Clip { name: "m2_matrix.ts", size: "640x352", secs: 2, args: &args, mux: &["-f", "mpegts"] }, 50, 12);
}

#[test]
fn mpeg2_q_scale_type_and_intra_vlc() {
    check(Clip { name: "m2_qs.mpg", size: "640x352", secs: 2, args: &["mpeg2video", "-b:v", "2M", "-g", "15", "-bf", "2", "-non_linear_quant", "1", "-qmax", "28", "-intra_vlc", "1", "-alternate_scan", "1"], mux: &["-f", "mpeg"] }, 50, 12);
}

#[test]
fn mpeg2_720p_ts() {
    check(Clip { name: "m2_720.ts", size: "1280x720", secs: 2, args: &["mpeg2video", "-b:v", "8M", "-g", "12", "-bf", "2"], mux: &["-f", "mpegts"] }, 50, 12);
}

#[test]
fn mpeg2_raw_elementary_stream() {
    check(Clip { name: "m2_raw.m2v", size: "640x352", secs: 2, args: &["mpeg2video", "-b:v", "3M", "-g", "12", "-bf", "2", "-an"], mux: &["-an", "-f", "mpeg2video"] }, 50, 12);
}

#[test]
fn mpeg2_mxf() {
    check(Clip { name: "m2.mxf", size: "720x576", secs: 2, args: &["mpeg2video", "-b:v", "30M", "-g", "1", "-bf", "0", "-minrate", "30M", "-maxrate", "30M", "-bufsize", "1200k", "-flags", "+ildct+ilme", "-an"], mux: &["-an", "-f", "mxf"] }, 50, 12);
}

#[test]
fn mpeg2_mxf_long_gop() {
    check(Clip { name: "m2_long.mxf", size: "720x576", secs: 2, args: &["mpeg2video", "-b:v", "8M", "-g", "6", "-bf", "2"], mux: &["-an", "-f", "mxf"] }, 50, 12);
}

#[test]
fn mpeg1_is_refused() {
    // the GPU's MPEG-1 mode garbles the picture (see mpeg2dec.rs): refused, not shown wrong
    if !have_ffmpeg() {
        return;
    }
    let clip = make_clip(&Clip { name: "m1.mpg", size: "352x288", secs: 3, args: &["mpeg1video", "-b:v", "1150k", "-g", "12", "-bf", "2"], mux: &["-f", "mpeg"] });
    match VideoDecoder::open(&clip) {
        Err(rustdecoder::Error::Unsupported(m)) => eprintln!("MPEG-1 refused: {m}"),
        Err(rustdecoder::Error::Gpu(m)) => eprintln!("skipped: {m}"),
        other => panic!("MPEG-1 should be refused, got {:?}", other.map(|d| d.codec_name())),
    }
}

/// The MP2 sound of a program stream through Windows' MPEG audio decoder, against ffmpeg's.
#[test]
fn mp2_audio_in_a_program_stream() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let clip = make_clip(&Clip { name: "m2_prog_b.mpg", size: "640x352", secs: 3, args: &["mpeg2video", "-b:v", "4M", "-g", "12", "-bf", "2"], mux: &["-f", "mpeg"] });
    let mut dec = match rustdecoder::audio::AudioDecoder::open(&clip) {
        Ok(Some(d)) => d,
        Ok(None) => panic!("no audio track"),
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    let (rate, ch) = (dec.info().sample_rate, dec.info().channels as usize);
    let (start, ours) = dec.decode_all().unwrap();
    let out = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&clip).args(["-map", "0:a:0", "-f", "f32le", "-ac", &ch.to_string(), "-"]).output().expect("ffmpeg");
    let theirs: Vec<f32> = out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    // the decoders may start a few frames apart (priming): find the shift of the best match
    let mut best = (f64::MAX, 0i64);
    for shift in -1200i64..=1200 {
        let mut e = 0f64;
        for i in (20000..24000).step_by(ch) {
            let j = i as i64 - shift * ch as i64;
            if j < 0 || j as usize >= theirs.len() || i >= ours.len() {
                continue;
            }
            let d = (ours[i] - theirs[j as usize]) as f64;
            e += d * d;
        }
        if e < best.0 {
            best = (e, shift);
        }
    }
    let n = ours.len().min(theirs.len()) as i64 - 2400 * ch as i64;
    let (mut max, mut sum, mut cnt) = (0f32, 0f64, 0u64);
    for i in 4800 * ch as i64..n {
        let j = i - best.1 * ch as i64;
        if j < 0 || j as usize >= theirs.len() {
            continue;
        }
        let d = (ours[i as usize] - theirs[j as usize]).abs();
        max = max.max(d);
        sum += (d as f64) * (d as f64);
        cnt += 1;
    }
    let rms = (sum / cnt.max(1) as f64).sqrt();
    eprintln!("MP2: {rate} Hz, {ch} ch, starts at {start:.4} s, {} frames ours / {} ffmpeg, best shift {} frames, max |diff| {max:.5}, rms {rms:.6}", ours.len() / ch, theirs.len() / ch, best.1);
    assert!(rms < 0.01, "MP2 decode differs: rms {rms}");
}

#[test]
fn mpeg2_1080_height_is_cropped_from_1088() {
    check(Clip { name: "m2_1080.ts", size: "1920x1080", secs: 1, args: &["mpeg2video", "-b:v", "15M", "-g", "12", "-bf", "2", "-flags", "+ilme+ildct"], mux: &["-f", "mpegts"] }, 25, 12);
}

/// 4:2:2 (XDCAM HD422, D-10 50) has no decoder profile in D3D11: refused with a message.
#[test]
fn mpeg2_422_is_refused() {
    if !have_ffmpeg() {
        return;
    }
    let out = temp_dir().join("m2_422.mpg");
    if !out.exists() {
        let status = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x352:rate=25", "-t", "1", "-pix_fmt", "yuv422p", "-c:v", "mpeg2video", "-b:v", "20M", "-f", "mpeg"])
            .arg(&out)
            .status()
            .expect("ffmpeg");
        assert!(status.success());
    }
    match VideoDecoder::open(&out) {
        Err(rustdecoder::Error::Unsupported(m)) => eprintln!("4:2:2 refused: {m}"),
        Err(rustdecoder::Error::Gpu(m)) => eprintln!("skipped: {m}"),
        other => panic!("4:2:2 should be refused, got {:?}", other.map(|d| d.codec_name())),
    }
}

/// Seeking to a key sample: the pictures after it are the ones a full decode gives (pictures
/// that need the GOP before are dropped).
#[test]
fn mpeg2_seek_to_a_key_sample() {
    if !have_ffmpeg() {
        return;
    }
    let clip = make_clip(&Clip { name: "m2_prog_b.mpg", size: "640x352", secs: 3, args: &["mpeg2video", "-b:v", "4M", "-g", "12", "-bf", "2"], mux: &["-f", "mpeg"] });
    let mut dec = match VideoDecoder::open(&clip) {
        Ok(d) => d,
        Err(rustdecoder::Error::Gpu(m)) => {
            eprintln!("skipped: {m}");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let mut all = std::collections::HashMap::new();
    let mut order = Vec::new();
    while let Some(f) = dec.next_frame().unwrap() {
        let p = dec.read_back(&f).unwrap();
        all.insert(f.pts, p.y.iter().fold(0u64, |h, v| h.wrapping_mul(31).wrapping_add(*v as u64)));
        order.push(f.pts);
    }
    let keys: Vec<usize> = dec.track().samples.iter().enumerate().filter(|(_, s)| s.key).map(|(i, _)| i).collect();
    assert!(keys.len() >= 5, "{} key samples", keys.len());
    let k = keys[3];
    let key_pts = dec.track().samples[k].pts;
    dec.seek_to_sample(k);
    let mut n = 0;
    let mut first = None;
    while let Some(f) = dec.next_frame().unwrap() {
        let p = dec.read_back(&f).unwrap();
        let h = p.y.iter().fold(0u64, |h, v| h.wrapping_mul(31).wrapping_add(*v as u64));
        assert_eq!(all.get(&f.pts), Some(&h), "picture at {} after the seek differs from the full decode", f.pts);
        first.get_or_insert(f.pts);
        n += 1;
    }
    eprintln!("seek to key sample {k} (pts {key_pts}): {n} pictures, first at {:?}, all equal to the full decode", first);
    assert_eq!(first, Some(key_pts));
    assert!(n >= 75 - 36 - 2);
}

/// Cut and damaged files never panic (the reader and the decoder).
#[test]
fn mpeg2_damaged_streams_do_not_panic() {
    if !have_ffmpeg() {
        return;
    }
    let clip = make_clip(&Clip { name: "m2_prog_b.mpg", size: "640x352", secs: 3, args: &["mpeg2video", "-b:v", "4M", "-g", "12", "-bf", "2"], mux: &["-f", "mpeg"] });
    let good = std::fs::read(&clip).unwrap();
    let mut seed = 12345u64;
    let mut rnd = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };
    for round in 0..6 {
        let mut d = good.clone();
        if round < 2 {
            d.truncate(good.len() * (round + 1) / 3);
        } else {
            for _ in 0..40 {
                let i = 2048 + rnd() % (d.len() - 2048);
                d[i] ^= 1 << (rnd() % 8);
            }
        }
        let p = temp_dir().join(format!("m2_damaged_{round}.mpg"));
        std::fs::write(&p, &d).unwrap();
        let Ok(mut dec) = VideoDecoder::open(&p) else { continue };
        let mut frames = 0;
        for _ in 0..200 {
            match dec.next_frame() {
                Ok(Some(_)) => frames += 1,
                Ok(None) => break,
                Err(_) => break,
            }
        }
        eprintln!("damaged {round}: {frames} frames, no panic");
        let _ = std::fs::remove_file(&p);
    }
}
