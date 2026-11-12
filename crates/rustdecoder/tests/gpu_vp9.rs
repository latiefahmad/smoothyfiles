//! VP9 GPU decoding compared with ffmpeg's software decoder (libavcodec's native VP9 decoder is
//! bit-exact with libvpx): every sample of every shown frame, in order, has to match. Also H.264
//! and HEVC inside Matroska. Clips are made with ffmpeg + libvpx into a temp folder (nothing is
//! kept in git); tests skip with a message when ffmpeg or a GPU decoder is missing.

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
    pix_fmt: &'a str,
    /// Extra video filters after the noise.
    vf: &'a str,
    /// Encoder arguments after `-c:v libvpx-vp9`.
    args: &'a [&'a str],
    /// Two passes (libvpx only uses hidden alternate reference frames and `show_existing_frame`
    /// with the first pass's statistics).
    two_pass: bool,
}

impl<'a> Clip<'a> {
    fn new(name: &'a str, args: &'a [&'a str]) -> Self {
        Self { name, size: "640x360", secs: 3, pix_fmt: "yuv420p", vf: "", args, two_pass: false }
    }
}

const BASE: &[&str] = &["-b:v", "1M", "-deadline", "good", "-cpu-used", "4", "-row-mt", "1"];

fn vp9_args(extra: &[&str]) -> Vec<String> {
    BASE.iter().chain(extra).map(|s| s.to_string()).collect()
}

/// Makes (once) a WebM clip of noisy test pattern with libvpx.
fn make_clip(c: &Clip) -> PathBuf {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let out = temp_dir().join(c.name);
    if out.exists() {
        return out;
    }
    let tmp = out.with_file_name(format!("part-{}", c.name));
    let vf = match c.vf {
        "" => "noise=alls=6:allf=t".to_string(),
        "clean" => "null".to_string(),
        f => format!("noise=alls=6:allf=t,{f}"),
    };
    let run = |pass: Option<&str>, log: &Path, sink: &Path, fmt: &str| {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg(format!("testsrc2=size={}:rate=30", c.size))
            .args(["-t", &c.secs.to_string(), "-vf", &vf, "-pix_fmt", c.pix_fmt, "-c:v", "libvpx-vp9"])
            .args(c.args);
        if let Some(p) = pass {
            cmd.args(["-pass", p, "-passlogfile"]).arg(log);
        }
        cmd.args(["-f", fmt]).arg(sink).status().expect("ffmpeg")
    };
    let log = temp_dir().join(format!("{}.pass", c.name));
    let status = if c.two_pass {
        let first = run(Some("1"), &log, Path::new("-"), "null");
        assert!(first.success(), "ffmpeg first pass of {}", c.name);
        let second = run(Some("2"), &log, &tmp, "webm");
        let _ = std::fs::remove_file(temp_dir().join(format!("{}.pass-0.log", c.name)));
        second
    } else {
        run(None, &log, &tmp, "webm")
    };
    assert!(status.success(), "ffmpeg could not make {}", c.name);
    std::fs::rename(&tmp, &out).unwrap();
    out
}

#[derive(Debug)]
struct Report {
    frames: usize,
    first_mismatch: Option<String>,
    fps: f64,
    sizes: Vec<(u32, u32)>,
}

/// Decodes on the GPU and compares each frame with ffmpeg's output, read from a pipe as it is
/// produced. `None` when there is no GPU decoder for the clip.
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
    let ten = info.bit_depth == 10;
    let mut ff = Command::new("ffmpeg")
        .args(["-v", "error", "-noautorotate", "-i"])
        .arg(clip)
        .args(["-noautoscale", "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", if ten { "p010le" } else { "nv12" }, "-"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("ffmpeg");
    let mut out = ff.stdout.take().unwrap();
    let mut report = Report { frames: 0, first_mismatch: None, fps: 0.0, sizes: Vec::new() };
    let t0 = std::time::Instant::now();
    let bytes = if ten { 2 } else { 1 };
    let mut ref_frame = Vec::new();
    loop {
        let Some(f) = dec.next_frame().unwrap() else { break };
        let planes = dec.read_back(&f).unwrap();
        let (w, h) = (planes.width as usize, planes.height as usize);
        if report.sizes.last() != Some(&(w as u32, h as u32)) {
            report.sizes.push((w as u32, h as u32));
        }
        let y_len = w * h * bytes;
        ref_frame.resize(y_len + w * (h / 2) * bytes, 0);
        if out.read_exact(&mut ref_frame).is_err() {
            report.first_mismatch = Some(format!("ffmpeg has fewer frames than we decoded (we are at {})", report.frames));
            break;
        }
        let (ry, ruv) = ref_frame.split_at(y_len);
        let at = |plane: &[u8], i: usize| -> u16 {
            if ten { u16::from_le_bytes([plane[2 * i], plane[2 * i + 1]]) >> 6 } else { plane[i] as u16 }
        };
        let bad = planes.y.iter().enumerate().find(|(i, v)| **v != at(ry, *i)).map(|(i, v)| format!("Y x={} y={} gpu={} ffmpeg={}", i % w, i / w, v, at(ry, i)));
        let bad = bad.or_else(|| planes.uv.iter().enumerate().find(|(i, v)| **v != at(ruv, *i)).map(|(i, v)| format!("UV x={} y={} gpu={} ffmpeg={}", i % w, i / w, v, at(ruv, i))));
        if let Some(b) = bad {
            report.first_mismatch.get_or_insert(format!("frame {} (pts {}, {w}x{h}): {b}", report.frames, f.pts));
        }
        report.frames += 1;
    }
    report.fps = report.frames as f64 / t0.elapsed().as_secs_f64();
    if report.first_mismatch.is_none() {
        let mut extra = [0u8; 1];
        if out.read(&mut extra).unwrap_or(0) > 0 {
            report.first_mismatch = Some(format!("ffmpeg has more frames than the {} we decoded", report.frames));
        }
    }
    let _ = ff.kill();
    let _ = ff.wait();
    Some(report)
}

fn check_path(name: &str, clip: &Path, frames: usize) {
    let Some(r) = compare(clip) else { return };
    eprintln!("{name}: {} frames compared with ffmpeg ({:.0} fps incl. read-back), sizes {:?}: {:?}", r.frames, r.fps, r.sizes, r.first_mismatch);
    assert_eq!(r.frames, frames, "{name}: frame count");
    assert_eq!(r.first_mismatch, None, "{name}");
}

fn check(c: Clip, frames: usize) {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let clip = make_clip(&c);
    check_path(c.name, &clip, frames);
}

#[test]
fn profile0_defaults() {
    check(Clip::new("vp9_default.webm", &["-b:v", "1M", "-cpu-used", "4"]), 90);
}

#[test]
fn profile0_one_pass_with_lookahead() {
    let a = vp9_args(&["-lag-in-frames", "25", "-auto-alt-ref", "1", "-g", "60"]);
    let a: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
    check(Clip::new("vp9_altref.webm", &a), 90);
}

#[test]
fn profile0_two_pass_superframes() {
    let mut c = Clip::new("vp9_2pass.webm", &["-b:v", "800k", "-auto-alt-ref", "1", "-lag-in-frames", "25", "-g", "90", "-cpu-used", "2"]);
    c.two_pass = true;
    check(c, 90);
}

#[test]
fn profile0_multi_layer_alt_ref_show_existing() {
    let mut c = Clip::new("vp9_multiarf.webm", &["-b:v", "300k", "-auto-alt-ref", "6", "-enable-tpl", "1", "-lag-in-frames", "25", "-g", "150", "-cpu-used", "1"]);
    c.vf = "clean";
    c.two_pass = true;
    check(c, 90);
}

#[test]
fn profile2_10_bit_multi_layer_alt_ref() {
    let mut c = Clip::new("vp9_10bit_multiarf.webm", &["-profile:v", "2", "-b:v", "400k", "-auto-alt-ref", "6", "-enable-tpl", "1", "-lag-in-frames", "25", "-g", "150", "-cpu-used", "2"]);
    c.pix_fmt = "yuv420p10le";
    c.two_pass = true;
    check(c, 90);
}

#[test]
fn profile0_no_alt_ref() {
    let a = vp9_args(&["-lag-in-frames", "0", "-auto-alt-ref", "0", "-g", "30"]);
    let a: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
    check(Clip::new("vp9_noaltref.webm", &a), 90);
}

#[test]
fn profile0_tile_columns_720p() {
    let a = vp9_args(&["-tile-columns", "2", "-lag-in-frames", "16", "-auto-alt-ref", "1"]);
    let a: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
    let mut c = Clip::new("vp9_tiles720.webm", &a);
    c.size = "1280x720";
    check(c, 90);
}

#[test]
fn profile0_tile_rows() {
    let a = vp9_args(&["-tile-columns", "1", "-tile-rows", "2"]);
    let a: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
    check(Clip::new("vp9_tilerows.webm", &a), 90);
}

#[test]
fn profile0_error_resilient_and_frame_parallel() {
    let a = vp9_args(&["-error-resilient", "1", "-frame-parallel", "1"]);
    let a: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
    check(Clip::new("vp9_errres.webm", &a), 90);
}

#[test]
fn profile0_segmentation_aq() {
    let a = vp9_args(&["-aq-mode", "1", "-lag-in-frames", "10", "-auto-alt-ref", "1"]);
    let a: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
    check(Clip::new("vp9_aq1.webm", &a), 90);
    let a = vp9_args(&["-aq-mode", "3", "-deadline", "realtime", "-lag-in-frames", "0"]);
    let a: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
    check(Clip::new("vp9_aq3.webm", &a), 90);
}

#[test]
fn profile0_lossless() {
    check(Clip::new("vp9_lossless.webm", &["-lossless", "1", "-cpu-used", "5"]), 90);
}

#[test]
fn profile0_odd_size() {
    let mut c = Clip::new("vp9_odd.webm", &["-b:v", "800k", "-cpu-used", "5"]);
    c.size = "642x362";
    check(c, 90);
}

#[test]
fn profile0_loop_filter_sharpness_and_static_thresh() {
    check(Clip::new("vp9_sharp.webm", &["-b:v", "600k", "-cpu-used", "3", "-sharpness", "5", "-static-thresh", "200", "-lag-in-frames", "12", "-auto-alt-ref", "1"]), 90);
}

#[test]
fn profile2_10_bit() {
    let a = vp9_args(&["-profile:v", "2", "-lag-in-frames", "20", "-auto-alt-ref", "1"]);
    let a: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
    let mut c = Clip::new("vp9_10bit.webm", &a);
    c.pix_fmt = "yuv420p10le";
    check(c, 90);
}

/// Two streams of different sizes joined with a key frame in between (a size change inside the
/// stream): the smaller after the larger keeps the surfaces, the larger after the smaller makes
/// new ones.
fn joined(name: &str, first: &str, second: &str) -> Option<PathBuf> {
    let out = temp_dir().join(name);
    if out.exists() {
        return Some(out);
    }
    let mk = |size: &str| {
        let name = format!("vp9_join_{size}.webm");
        let mut c = Clip::new(&name, &["-b:v", "500k", "-cpu-used", "5", "-g", "30"]);
        c.size = size;
        c.secs = 2;
        make_clip(&c)
    };
    let (a, b) = (mk(first), mk(second));
    let list = temp_dir().join(format!("{name}.txt"));
    let q = |p: &Path| format!("file '{}'\n", p.to_string_lossy().replace('\\', "/"));
    std::fs::write(&list, format!("{}{}", q(&a), q(&b))).unwrap();
    let tmp = out.with_file_name(format!("part-{name}"));
    let status = Command::new("ffmpeg").args(["-y", "-v", "error", "-f", "concat", "-safe", "0", "-i"]).arg(&list).args(["-c", "copy", "-f", "webm"]).arg(&tmp).status().expect("ffmpeg");
    let _ = std::fs::remove_file(&list);
    if !status.success() {
        return None;
    }
    std::fs::rename(&tmp, &out).unwrap();
    Some(out)
}

#[test]
fn size_change_at_a_key_frame_larger_to_smaller() {
    if !have_ffmpeg() {
        return;
    }
    let Some(p) = joined("vp9_big_small.webm", "640x360", "320x180") else { return };
    check_path("big_small", &p, 120);
}

#[test]
fn size_change_at_a_key_frame_smaller_to_larger() {
    if !have_ffmpeg() {
        return;
    }
    let Some(p) = joined("vp9_small_big.webm", "320x180", "640x360") else { return };
    check_path("small_big", &p, 120);
}

#[test]
fn h264_in_mkv() {
    if !have_ffmpeg() {
        return;
    }
    let p = common::make_mkv("gpu_h264.mkv", &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-preset", "veryfast", "-x264-params", "log-level=error:keyint=30:bframes=3:ref=4"], &["-c:a", "aac"], &[]);
    check_path("h264 in mkv", &p, 90);
}

#[test]
fn hevc_in_mkv() {
    if !have_ffmpeg() {
        return;
    }
    let p = common::make_mkv("gpu_hevc.mkv", &["-c:v", "libx265", "-pix_fmt", "yuv420p", "-preset", "ultrafast", "-x265-params", "log-level=error:keyint=30:bframes=3"], &["-c:a", "pcm_s16le"], &[]);
    check_path("hevc in mkv", &p, 90);
}

#[test]
fn profile1_444_is_refused_clearly() {
    if !have_ffmpeg() {
        return;
    }
    let mut c = Clip::new("vp9_444.webm", &["-profile:v", "1", "-b:v", "1M", "-cpu-used", "5"]);
    c.pix_fmt = "yuv444p";
    let clip = make_clip(&c);
    match VideoDecoder::open(&clip) {
        Err(rustdecoder::Error::Unsupported(m)) => eprintln!("4:4:4 refused: {m}"),
        other => panic!("4:4:4 was not refused: {:?}", other.map(|d| d.codec_name())),
    }
}

/// What the generated clips contain, so the bit-exact results above mean something: counts of
/// the header features over every frame of every clip made by the tests.
#[test]
fn features_in_the_clips() {
    use rustdecoder::vp9::{Parser, split_superframe};
    if !have_ffmpeg() {
        return;
    }
    let mut names: Vec<_> = std::fs::read_dir(temp_dir()).unwrap().flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("vp9_") && n.to_string_lossy().ends_with(".webm"))).collect();
    names.sort();
    for p in names {
        let Ok(m) = rustdecoder::container::read(&mut std::fs::File::open(&p).unwrap()) else { continue };
        let Some(t) = m.video() else { continue };
        let mut f = std::fs::File::open(&p).unwrap();
        let mut buf = Vec::new();
        let mut parser = Parser::new();
        let (mut packets, mut super_, mut hidden, mut existing, mut intra_only, mut seg, mut tiles, mut inter, mut key) = (0, 0, 0, 0, 0, 0, 0, 0, 0);
        let mut filters = [0u32; 5];
        for i in 0..t.samples.len() {
            t.read_sample(&mut f, i, &mut buf).unwrap();
            let frames = split_superframe(&buf).unwrap();
            packets += 1;
            if frames.len() > 1 {
                super_ += 1;
            }
            for fr in frames {
                let h = parser.parse(fr).unwrap();
                if h.show_existing.is_some() {
                    existing += 1;
                    continue;
                }
                parser.refresh(h.refresh_frame_flags, h.width, h.height);
                hidden += !h.show_frame as u32;
                intra_only += h.intra_only as u32;
                seg += h.seg.enabled as u32;
                tiles += (h.log2_tile_cols > 0 || h.log2_tile_rows > 0) as u32;
                key += h.key_frame as u32;
                inter += (!h.is_intra()) as u32;
                if !h.is_intra() {
                    filters[h.interp_filter as usize] += 1;
                }
            }
        }
        eprintln!("{}: {packets} packets, {super_} superframes, {key} key, {inter} inter, {intra_only} intra-only, {hidden} hidden, {existing} show-existing, {seg} with segmentation, {tiles} tiled, filters {filters:?}", p.file_name().unwrap().to_string_lossy());
    }
}

/// Any clip named in RD_CLIP, for debugging (does nothing without it).
#[test]
fn clip_from_env() {
    let Some(p) = std::env::var_os("RD_CLIP") else { return };
    let Some(r) = compare(Path::new(&p)) else { return };
    eprintln!("RD_CLIP: {} frames, first mismatch {:?}, sizes {:?}", r.frames, r.first_mismatch, r.sizes);
}
