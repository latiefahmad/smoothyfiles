//! ProRes on the CPU: every frame of clips made by ffmpeg's two encoders is compared with what
//! ffmpeg's own decoder gives (`-f rawvideo`), plane by plane.
//!
//! The entropy decoding, scans, dequantisation and block layout are bit exact. The inverse DCT
//! is an accurate integer one that differs from ffmpeg's own in the last bit for about one
//! sample in a thousand (RDD 36 allows that); the test requires a maximum error of 1.
mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Instant;

use common::{have_ffmpeg, temp_dir};
use rustdecoder::prores::{self, Picture, ProResDecoder};

static LOCK: Mutex<()> = Mutex::new(());

struct Spec {
    name: &'static str,
    size: (u32, u32),
    /// ffmpeg encoder arguments.
    enc: &'static [&'static str],
    /// Input pixel format of the encoder (what the clip is made from).
    pix_fmt: &'static str,
    /// What to ask ffmpeg's decoder for.
    ref_fmt: &'static str,
    frames: u32,
}

fn make(s: &Spec) -> PathBuf {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let out = temp_dir().join(format!("{}_{}x{}.mov", s.name, s.size.0, s.size.1));
    if out.exists() {
        return out;
    }
    let tmp = out.with_extension("part.mov");
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size={}x{}:rate=25", s.size.0, s.size.1))
        .args(["-frames:v", &s.frames.to_string(), "-pix_fmt", s.pix_fmt])
        .args(s.enc)
        .args(["-f", "mov"])
        .arg(&tmp)
        .status()
        .expect("ffmpeg");
    assert!(status.success(), "ffmpeg could not make {}", s.name);
    std::fs::rename(&tmp, &out).unwrap();
    out
}

fn reference(path: &Path, fmt: &str) -> Vec<u8> {
    let o = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(path).args(["-f", "rawvideo", "-pix_fmt", fmt, "-"]).stderr(Stdio::inherit()).output().expect("ffmpeg");
    assert!(o.status.success());
    o.stdout
}

fn decode_all(path: &Path, threads: usize) -> (Vec<Picture>, [u8; 4]) {
    let mut f = File::open(path).unwrap();
    let movie = rustdecoder::container::read(&mut f).unwrap();
    let track = movie.video().unwrap();
    let bits = prores::bits_for_fourcc(&track.fourcc);
    let dec = ProResDecoder::new(threads);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    for i in 0..track.samples.len() {
        track.read_sample(&mut f, i, &mut buf).unwrap();
        let mut p = Picture::default();
        dec.decode(&buf, bits, &mut p).unwrap();
        out.push(p);
    }
    (out, track.fourcc)
}

/// (max error, mean error, count of different samples) of one plane.
fn plane_err(ours: &[u16], stride: usize, w: usize, h: usize, theirs: &[u8]) -> (i32, f64, usize) {
    let mut max = 0;
    let mut sum = 0u64;
    let mut n = 0;
    for y in 0..h {
        for x in 0..w {
            let a = ours[y * stride + x] as i32;
            let i = (y * w + x) * 2;
            let b = u16::from_le_bytes([theirs[i], theirs[i + 1]]) as i32;
            let d = (a - b).abs();
            max = max.max(d);
            sum += d as u64;
            if d != 0 {
                n += 1;
            }
        }
    }
    (max, sum as f64 / (w * h) as f64, n)
}

/// Compares every frame and returns a row of the table plus the largest error found.
fn compare(s: &Spec) -> (String, i32) {
    let path = make(s);
    let (pics, fourcc) = decode_all(&path, 1);
    let (par, _) = decode_all(&path, 0);
    for (a, b) in pics.iter().zip(&par) {
        assert!(a.y == b.y && a.u == b.u && a.v == b.v, "{}: parallel decoding differs from single threaded", s.name);
    }
    if s.name.ends_with("tff") || s.name.ends_with("bff") {
        let mut f = File::open(&path).unwrap();
        let movie = rustdecoder::container::read(&mut f).unwrap();
        let mut buf = Vec::new();
        movie.video().unwrap().read_sample(&mut f, 0, &mut buf).unwrap();
        let hdr = prores::parse_header(&buf).unwrap();
        assert_eq!(hdr.interlace, if s.name.ends_with("tff") { 1 } else { 2 }, "{} is not interlaced as asked", s.name);
    }
    let theirs = reference(&path, s.ref_fmt);
    let alpha_planes = if s.ref_fmt.starts_with("yuva") { 1 } else { 0 };
    let (w, h) = (pics[0].width as usize, pics[0].height as usize);
    let (cw, ch) = {
        let (a, b) = pics[0].chroma_size();
        (a as usize, b as usize)
    };
    let frame_bytes = (w * h * (1 + alpha_planes) + 2 * cw * ch) * 2;
    assert_eq!(theirs.len(), frame_bytes * pics.len(), "{}: ffmpeg frame count", s.name);
    let mut worst = [(0, 0.0, 0usize); 3];
    for (i, p) in pics.iter().enumerate() {
        let f = &theirs[i * frame_bytes..][..frame_bytes];
        let e = [plane_err(&p.y, p.stride_y, w, h, &f[..w * h * 2]), plane_err(&p.u, p.stride_c, cw, ch, &f[w * h * 2..][..cw * ch * 2]), plane_err(&p.v, p.stride_c, cw, ch, &f[(w * h + cw * ch) * 2..][..cw * ch * 2])];
        for k in 0..3 {
            worst[k].0 = worst[k].0.max(e[k].0);
            worst[k].1 += e[k].1 / pics.len() as f64;
            worst[k].2 += e[k].2;
        }
    }
    let max = worst.iter().map(|e| e.0).max().unwrap();
    let total = (w * h + 2 * cw * ch) * pics.len();
    let fmt = |e: &(i32, f64, usize)| if e.2 == 0 { "exact".to_string() } else { format!("max {} mean {:.5}", e.0, e.1) };
    let diff: usize = worst.iter().map(|e| e.2).sum();
    let line = format!(
        "| {:<12} | {:>4}x{:<4} | {} | {:<10} | {} | {} | {} | {:.3}% |",
        s.name,
        w,
        h,
        String::from_utf8_lossy(&fourcc),
        s.ref_fmt,
        fmt(&worst[0]),
        fmt(&worst[1]),
        fmt(&worst[2]),
        100.0 * diff as f64 / total as f64
    );
    println!("{line}");
    (line, max)
}

fn ks422(profile: usize, size: (u32, u32)) -> Spec {
    const NAMES: [&str; 4] = ["ks_proxy", "ks_lt", "ks_422", "ks_hq"];
    const ENC: [&[&str]; 4] = [&["-c:v", "prores_ks", "-profile:v", "0"], &["-c:v", "prores_ks", "-profile:v", "1"], &["-c:v", "prores_ks", "-profile:v", "2"], &["-c:v", "prores_ks", "-profile:v", "3"]];
    Spec { name: NAMES[profile], size, enc: ENC[profile], pix_fmt: "yuv422p10le", ref_fmt: "yuv422p10le", frames: 3 }
}

fn check(specs: Vec<Spec>) {
    if !have_ffmpeg() {
        return;
    }
    println!("| clip | size | tag | compared as | Y | U | V | samples off by 1 |");
    for s in &specs {
        let (_, max) = compare(s);
        assert!(max <= 1, "{}: error {max}", s.name);
    }
}

#[test]
fn prores_422_profiles_640() {
    check((0..4).map(|i| ks422(i, (640, 360))).collect());
}

#[test]
fn prores_422_profiles_1080() {
    check((0..4).map(|i| ks422(i, (1920, 1080))).collect());
}

#[test]
fn prores_aw_encoder() {
    check(vec![
        Spec { name: "aw_proxy", size: (640, 360), enc: &["-c:v", "prores_aw", "-profile:v", "0"], pix_fmt: "yuv422p10le", ref_fmt: "yuv422p10le", frames: 3 },
        Spec { name: "aw_hq", size: (640, 360), enc: &["-c:v", "prores_aw", "-profile:v", "3"], pix_fmt: "yuv422p10le", ref_fmt: "yuv422p10le", frames: 3 },
        Spec { name: "aw_hq", size: (1920, 1080), enc: &["-c:v", "prores_aw", "-profile:v", "3"], pix_fmt: "yuv422p10le", ref_fmt: "yuv422p10le", frames: 3 },
    ]);
}

#[test]
fn prores_odd_sizes() {
    // not a multiple of 16: partial macroblocks, narrower last slices
    check(vec![
        Spec { name: "ks_hq", size: (650, 366), enc: &["-c:v", "prores_ks", "-profile:v", "3"], pix_fmt: "yuv422p10le", ref_fmt: "yuv422p10le", frames: 2 },
        Spec { name: "ks_lt", size: (322, 178), enc: &["-c:v", "prores_ks", "-profile:v", "1"], pix_fmt: "yuv422p10le", ref_fmt: "yuv422p10le", frames: 2 },
    ]);
}

#[test]
fn prores_4444() {
    check(vec![
        Spec { name: "ks_4444", size: (640, 360), enc: &["-c:v", "prores_ks", "-profile:v", "4"], pix_fmt: "yuv444p12le", ref_fmt: "yuv444p12le", frames: 3 },
        Spec { name: "ks_4444a", size: (640, 360), enc: &["-c:v", "prores_ks", "-profile:v", "4"], pix_fmt: "yuva444p12le", ref_fmt: "yuva444p12le", frames: 3 },
        Spec { name: "ks_4444", size: (1920, 1080), enc: &["-c:v", "prores_ks", "-profile:v", "4"], pix_fmt: "yuv444p12le", ref_fmt: "yuv444p12le", frames: 3 },
        Spec { name: "ks_4444xq", size: (640, 360), enc: &["-c:v", "prores_ks", "-profile:v", "5"], pix_fmt: "yuv444p12le", ref_fmt: "yuv444p12le", frames: 3 },
        Spec { name: "aw_4444", size: (640, 360), enc: &["-c:v", "prores_aw", "-profile:v", "4"], pix_fmt: "yuv444p10le", ref_fmt: "yuv444p12le", frames: 3 },
    ]);
}

/// Damaged frames (cut short, bytes flipped) give an error or some picture, never a panic.
#[test]
fn damaged_frames_do_not_panic() {
    if !have_ffmpeg() {
        return;
    }
    for spec in [ks422(3, (640, 360)), Spec { name: "ks_4444", size: (640, 360), enc: &["-c:v", "prores_ks", "-profile:v", "4"], pix_fmt: "yuv444p12le", ref_fmt: "", frames: 3 }] {
        let path = make(&spec);
        let mut f = File::open(&path).unwrap();
        let movie = rustdecoder::container::read(&mut f).unwrap();
        let track = movie.video().unwrap();
        let bits = prores::bits_for_fourcc(&track.fourcc);
        let mut frame = Vec::new();
        track.read_sample(&mut f, 0, &mut frame).unwrap();
        let dec = ProResDecoder::new(2);
        let mut pic = Picture::default();
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let (mut ok, mut err) = (0, 0);
        for round in 0..400 {
            let mut d = frame.clone();
            match round % 4 {
                0 => d.truncate((next() as usize) % d.len()),
                1 => {
                    for _ in 0..8 {
                        let i = (next() as usize) % d.len();
                        d[i] ^= 1 << (next() % 8);
                    }
                }
                2 => {
                    // damage the headers
                    for _ in 0..4 {
                        let i = (next() as usize) % 200.min(d.len());
                        d[i] = next() as u8;
                    }
                }
                _ => {
                    // damage inside the slice data
                    for _ in 0..16 {
                        let i = 300 + (next() as usize) % (d.len() - 300);
                        d[i] = next() as u8;
                    }
                }
            }
            match dec.decode(&d, bits, &mut pic) {
                Ok(_) => ok += 1,
                Err(_) => err += 1,
            }
        }
        println!("{}: {ok} damaged frames decoded to something, {err} refused, no panic", spec.name);
    }
}

#[test]
fn prores_interlaced() {
    check(vec![
        Spec { name: "ks_hq_tff", size: (640, 360), enc: &["-c:v", "prores_ks", "-profile:v", "3", "-flags", "+ilme+ildct", "-top", "1"], pix_fmt: "yuv422p10le", ref_fmt: "yuv422p10le", frames: 3 },
        Spec { name: "ks_hq_bff", size: (640, 360), enc: &["-c:v", "prores_ks", "-profile:v", "3", "-flags", "+ilme+ildct", "-top", "0"], pix_fmt: "yuv422p10le", ref_fmt: "yuv422p10le", frames: 3 },
        Spec { name: "ks_hq_tff", size: (1920, 1080), enc: &["-c:v", "prores_ks", "-profile:v", "3", "-flags", "+ilme+ildct", "-top", "1"], pix_fmt: "yuv422p10le", ref_fmt: "yuv422p10le", frames: 2 },
    ]);
}

/// The GPU path: a ProRes picture uploaded as planar R16 and drawn by the converter looks like
/// ffmpeg's own YUV to RGB conversion of the same clip (chroma upsampling differs a little).
#[cfg(windows)]
mod gpu {
    use super::*;
    use rustdecoder::VideoDecoder;
    use rustdecoder::present::{Colorimetry, Converter, Params, Source};
    use std::io::Read;

    const TAG: [&str; 6] = ["-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709"];

    fn present(s: &Spec, frames: usize) {
        let path = make(s);
        let mut dec = match VideoDecoder::open(&path) {
            Ok(d) => d,
            Err(rustdecoder::Error::Gpu(m)) => return eprintln!("skipped: {m}"),
            Err(e) => panic!("open: {e}"),
        };
        assert_eq!(dec.codec_name(), "ProRes");
        let info = dec.info().clone();
        let mut conv = Converter::new(&dec.gpu().device, &dec.gpu().context).unwrap();
        let params = Params { colour: Colorimetry::from_vui(&info.vui, info.width, info.height), rotation: 0, flip: false };
        let target = conv.create_target(info.width, info.height).unwrap();
        let mut ff = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&path)
            .args(["-frames:v", &frames.to_string(), "-vf", "scale=in_color_matrix=bt709:in_range=tv:out_range=full:flags=bilinear+accurate_rnd+full_chroma_int", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("ffmpeg");
        let mut out = ff.stdout.take().unwrap();
        let mut refbuf = vec![0u8; (info.width * info.height * 3) as usize];
        let (mut sum, mut max, mut n) = (0u64, 0u8, 0usize);
        for _ in 0..frames {
            let f = dec.next_frame().unwrap().expect("frame");
            conv.convert(&Source::from_frame(&f), &params, &target).unwrap();
            let ours = conv.read_rgba(&target).unwrap();
            out.read_exact(&mut refbuf).expect("ffmpeg frame");
            for (a, b) in ours.chunks_exact(4).zip(refbuf.chunks_exact(3)) {
                for c in 0..3 {
                    let d = a[c].abs_diff(b[c]);
                    sum += d as u64;
                    max = max.max(d);
                }
            }
            n += 1;
        }
        let _ = ff.kill();
        let _ = ff.wait();
        let mae = sum as f64 / (n as f64 * info.width as f64 * info.height as f64 * 3.0);
        println!("{} {}x{} on the GPU vs ffmpeg RGB: mean abs error {mae:.3}, max {max}", s.name, info.width, info.height);
        assert!(mae < 2.0, "{mae}");
    }

    #[test]
    fn prores_422_presents_like_ffmpeg() {
        if !have_ffmpeg() {
            return;
        }
        let mut enc = vec!["-c:v", "prores_ks", "-profile:v", "3"];
        enc.extend(TAG);
        let enc: &'static [&'static str] = Box::leak(enc.into_boxed_slice());
        present(&Spec { name: "gpu_422", size: (640, 360), enc, pix_fmt: "yuv422p10le", ref_fmt: "", frames: 4 }, 4);
        present(&Spec { name: "gpu_422_odd", size: (651, 367), enc, pix_fmt: "yuv422p10le", ref_fmt: "", frames: 3 }, 3);
    }

    /// `VideoDecoder::next_frame` end to end: decode on the pool, pack, upload to the GPU.
    #[test]
    fn prores_video_decoder_speed() {
        if !have_ffmpeg() {
            return;
        }
        for (label, name, size, frames, enc, fmt) in [
            ("1080p 422 HQ", "ks_hq_noisy_30", (1920u32, 1080u32), 30u32, &["-vf", "noise=alls=20:allf=t", "-c:v", "prores_ks", "-profile:v", "3"][..], "yuv422p10le"),
            ("2160p 422 HQ", "ks_hq_noisy_10", (3840, 2160), 10, &["-vf", "noise=alls=20:allf=t", "-c:v", "prores_ks", "-profile:v", "3"][..], "yuv422p10le"),
        ] {
            let path = make(&Spec { name, size, enc, pix_fmt: fmt, ref_fmt: "", frames });
            let mut dec = match VideoDecoder::open(&path) {
                Ok(d) => d,
                Err(rustdecoder::Error::Gpu(m)) => return eprintln!("skipped: {m}"),
                Err(e) => panic!("open: {e}"),
            };
            dec.next_frame().unwrap().unwrap();
            dec.seek_to_sample(0);
            let t = Instant::now();
            let mut n = 0;
            for _ in 0..3 {
                dec.seek_to_sample(0);
                while dec.next_frame().unwrap().is_some() {
                    n += 1;
                }
            }
            dec.wait_idle().unwrap();
            let fps = n as f64 / t.elapsed().as_secs_f64();
            println!("{label}: VideoDecoder::next_frame (decode, pack, upload): {fps:.1} fps on {}", dec.adapter());
            assert!(fps > 12.0, "{fps}"); // real time is 25; other tests run beside this one
        }
    }

    #[test]
    fn prores_4444_presents_like_ffmpeg() {
        if !have_ffmpeg() {
            return;
        }
        let mut enc = vec!["-c:v", "prores_ks", "-profile:v", "4"];
        enc.extend(TAG);
        let enc: &'static [&'static str] = Box::leak(enc.into_boxed_slice());
        present(&Spec { name: "gpu_4444", size: (640, 360), enc, pix_fmt: "yuv444p12le", ref_fmt: "", frames: 4 }, 4);
    }
}

/// Decoding speed of noisy (so, heavy) clips: frames per second with one thread and with several
/// (printed). The parallel run should keep real time (24 fps at 1080p, 25 fps at 4K); the assertion
/// allows half of that because tests run side by side.
fn speed(label: &str, spec: &Spec, must_reach: f64) {
    let path = make(spec);
    let mut f = File::open(&path).unwrap();
    let movie = rustdecoder::container::read(&mut f).unwrap();
    let track = movie.video().unwrap();
    let bits = prores::bits_for_fourcc(&track.fourcc);
    let mut frames = Vec::new();
    for i in 0..track.samples.len() {
        let mut b = Vec::new();
        track.read_sample(&mut f, i, &mut b).unwrap();
        frames.push(b);
    }
    let mb: f64 = frames.iter().map(|f| f.len() as f64).sum::<f64>() / frames.len() as f64 / 1e6;
    for threads in [1usize, 4, 8, 0] {
        let dec = ProResDecoder::new(threads);
        let mut pic = Picture::default();
        dec.decode(&frames[0], bits, &mut pic).unwrap();
        let t = Instant::now();
        let rounds = if threads == 1 { 1 } else { 3 };
        for _ in 0..rounds {
            for fr in &frames {
                dec.decode(fr, bits, &mut pic).unwrap();
            }
        }
        let fps = (rounds * frames.len()) as f64 / t.elapsed().as_secs_f64();
        println!("{label} ({mb:.2} MB per frame, {:.0} Mbit/s at 25 fps): {:>2} thread(s): {fps:.1} fps", mb * 8.0 * 25.0, dec.threads());
        if threads == 0 {
            assert!(fps > must_reach / 2.0, "too slow: {fps}"); // half the target: other tests run beside this one
        }
    }
}

#[test]
fn prores_speed_1080p_hq() {
    if !have_ffmpeg() {
        return;
    }
    speed("1080p 422 HQ", &Spec { name: "ks_hq_noisy_30", size: (1920, 1080), enc: &["-vf", "noise=alls=20:allf=t", "-c:v", "prores_ks", "-profile:v", "3"], pix_fmt: "yuv422p10le", ref_fmt: "", frames: 30 }, 24.0);
}

#[test]
fn prores_speed_4k_hq() {
    if !have_ffmpeg() {
        return;
    }
    speed("2160p 422 HQ", &Spec { name: "ks_hq_noisy_10", size: (3840, 2160), enc: &["-vf", "noise=alls=20:allf=t", "-c:v", "prores_ks", "-profile:v", "3"], pix_fmt: "yuv422p10le", ref_fmt: "", frames: 10 }, 25.0);
}

#[test]
fn prores_speed_4444_1080p() {
    if !have_ffmpeg() {
        return;
    }
    speed("1080p 4444 12 bit", &Spec { name: "ks_4444_noisy_20", size: (1920, 1080), enc: &["-vf", "noise=alls=20:allf=t", "-c:v", "prores_ks", "-profile:v", "4"], pix_fmt: "yuv444p12le", ref_fmt: "", frames: 20 }, 24.0);
}

