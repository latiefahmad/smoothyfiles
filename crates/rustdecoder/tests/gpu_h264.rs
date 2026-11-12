//! H.264 GPU decoding compared with ffmpeg's software decoder: H.264 decoding is exactly
//! specified, so every sample of every frame, in display order, has to match. Clips are made
//! with ffmpeg + x264 into a temp folder (nothing is kept in git); tests skip with a message
//! when ffmpeg or a GPU decoder is missing.

#![cfg(windows)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rustdecoder::VideoDecoder;

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn temp_dir() -> PathBuf {
    let d = std::env::temp_dir().join("rustdecoder-tests");
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Clip<'a> {
    name: &'a str,
    size: &'a str,
    secs: u32,
    /// Extra video filters after the noise.
    vf: &'a str,
    profile: &'a str,
    level: Option<&'a str>,
    x264: &'a str,
    /// Use these encoder arguments instead of libx264 (`-profile:v` and `x264` are ignored).
    encoder: &'a [&'a str],
}

impl<'a> Clip<'a> {
    fn new(name: &'a str, profile: &'a str, x264: &'a str) -> Self {
        Self { name, size: "640x360", secs: 3, vf: "", profile, level: None, x264, encoder: &[] }
    }
}

/// Makes (once) a clip of noisy test pattern with x264.
fn make_clip(c: &Clip) -> PathBuf {
    let out = temp_dir().join(c.name);
    if out.exists() {
        return out;
    }
    let tmp = out.with_extension("part.mp4");
    let vf = if c.vf.is_empty() { "noise=alls=6:allf=t".to_string() } else { format!("noise=alls=6:allf=t,{}", c.vf) };
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-v", "error", "-f", "lavfi", "-i"]).arg(format!("testsrc2=size={}:rate=30", c.size)).args(["-t", &c.secs.to_string(), "-vf", &vf]);
    if c.encoder.is_empty() {
        cmd.args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-profile:v", c.profile]);
        if let Some(l) = c.level {
            cmd.args(["-level", l]);
        }
        cmd.arg("-x264-params").arg(c.x264);
    } else {
        cmd.args(["-pix_fmt", "yuv420p"]).args(c.encoder);
    }
    cmd.arg(&tmp);
    let status = cmd.status().expect("ffmpeg");
    if !status.success() && !c.encoder.is_empty() {
        // a hardware encoder this machine does not have
        return PathBuf::new();
    }
    assert!(status.success(), "ffmpeg could not make {}", c.name);
    std::fs::rename(&tmp, &out).unwrap();
    out
}

#[derive(Debug)]
struct Report {
    frames: usize,
    first_mismatch: Option<String>,
    fps: f64,
    bad_frames: Vec<(usize, i32)>,
}

/// Decodes up to `max` frames on the GPU and compares each with ffmpeg's output, read from a
/// pipe as it is produced. `None` when there is no GPU decoder for the clip.
fn compare(clip: &Path, max: usize) -> Option<Report> {
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
        .args(["-v", "error", "-noautorotate", "-i"])
        .arg(clip)
        .args(["-frames:v", &max.to_string(), "-f", "rawvideo", "-pix_fmt", "nv12", "-"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("ffmpeg");
    let mut out = ff.stdout.take().unwrap();
    let (w, h) = (info.width as usize, info.height as usize);
    let y_len = w * h;
    let uv_len = w * (h / 2);
    let mut ref_frame = vec![0u8; y_len + uv_len];
    let mut report = Report { frames: 0, first_mismatch: None, fps: 0.0, bad_frames: Vec::new() };
    let t0 = std::time::Instant::now();
    while report.frames < max {
        let Some(f) = dec.next_frame().unwrap() else { break };
        let planes = dec.read_back(&f).unwrap();
        if out.read_exact(&mut ref_frame).is_err() {
            report.first_mismatch = Some(format!("ffmpeg has fewer frames than we decoded (we are at {})", report.frames));
            break;
        }
        assert_eq!((planes.width as usize, planes.height as usize), (w, h));
        let (ry, ruv) = ref_frame.split_at(y_len);
        let bad = planes.y.iter().enumerate().find(|(i, v)| **v != ry[*i] as u16).map(|(i, v)| format!("Y x={} y={} gpu={} ffmpeg={}", i % w, i / w, v, ry[i]));
        let bad = bad.or_else(|| planes.uv.iter().enumerate().find(|(i, v)| **v != ruv[*i] as u16).map(|(i, v)| format!("UV x={} y={} gpu={} ffmpeg={}", i % w, i / w, v, ruv[i])));
        if let Some(b) = bad {
            if std::env::var_os("RD_DEBUG").is_some() {
                let n_y = planes.y.iter().enumerate().filter(|(i, v)| **v != ry[*i] as u16).count();
                let n_uv = planes.uv.iter().enumerate().filter(|(i, v)| **v != ruv[*i] as u16).count();
                eprintln!("frame {} POC {}: {n_y} of {} Y and {n_uv} of {} UV samples differ; first {b}", report.frames, f.poc, planes.y.len(), planes.uv.len());
            }
            report.bad_frames.push((report.frames, f.poc));
            report.first_mismatch.get_or_insert(format!("frame {} (POC {}): {b}", report.frames, f.poc));
        }
        report.frames += 1;
    }
    report.fps = report.frames as f64 / t0.elapsed().as_secs_f64();
    if report.first_mismatch.is_none() && report.frames < max {
        let mut extra = [0u8; 1];
        if out.read(&mut extra).unwrap_or(0) > 0 {
            report.first_mismatch = Some(format!("ffmpeg has more frames than the {} we decoded", report.frames));
        }
    }
    let _ = ff.kill();
    let _ = ff.wait();
    Some(report)
}

fn check(c: Clip, frames: usize) {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let clip = make_clip(&c);
    if clip.as_os_str().is_empty() {
        eprintln!("skipped: ffmpeg cannot encode {} here", c.name);
        return;
    }
    let Some(r) = compare(&clip, 100_000) else { return };
    eprintln!("{}: {} frames compared with ffmpeg ({:.0} fps incl. read-back): {:?}", c.name, r.frames, r.fps, r.first_mismatch);
    assert_eq!(r.frames, frames, "{}: frame count", c.name);
    assert_eq!(r.first_mismatch, None, "{}", c.name);
}

#[test]
fn baseline_cavlc() {
    check(Clip::new("h264_base.mp4", "baseline", "keyint=30:ref=3"), 90);
}

#[test]
fn main_cabac_bframes_pyramid() {
    check(Clip::new("h264_main.mp4", "main", "bframes=3:b-pyramid=normal:keyint=30:ref=4"), 90);
}

#[test]
fn main_cavlc_bframes() {
    check(Clip::new("h264_main_cavlc.mp4", "main", "cabac=0:bframes=3:keyint=30:ref=3"), 90);
}

#[test]
fn high_8x8_transform() {
    check(Clip::new("h264_high.mp4", "high", "bframes=3:8x8dct=1:keyint=30:ref=4"), 90);
}

#[test]
fn high_custom_scaling_matrices() {
    check(Clip::new("h264_high_cqm.mp4", "high", "bframes=3:8x8dct=1:cqm=jvt:keyint=30"), 90);
}

#[test]
fn high_several_slices() {
    check(Clip::new("h264_slices.mp4", "high", "bframes=2:slices=4:keyint=30"), 90);
}

#[test]
fn weighted_prediction() {
    let mut c = Clip::new("h264_weightp.mp4", "high", "weightp=2:weightb=1:bframes=3:ref=3:keyint=60");
    c.vf = "fade=t=in:st=0:d=1.2,fade=t=out:st=1.6:d=1.2";
    check(c, 90);
}

#[test]
fn eight_reference_frames() {
    check(Clip::new("h264_ref8.mp4", "high", "ref=8:bframes=3:b-pyramid=normal:keyint=30"), 90);
}

#[test]
fn long_gop() {
    let mut c = Clip::new("h264_long.mp4", "high", "keyint=250:min-keyint=250:scenecut=0:bframes=3:b-pyramid=normal:ref=4");
    c.secs = 9;
    check(c, 270);
}

#[test]
fn open_gop() {
    check(Clip::new("h264_open.mp4", "high", "open-gop=1:keyint=24:min-keyint=24:bframes=4:b-pyramid=normal:ref=4"), 90);
}

#[test]
fn strict_pyramid_mmco() {
    check(Clip::new("h264_strict.mp4", "high", "bframes=5:b-pyramid=strict:keyint=40:ref=5"), 90);
}

#[test]
fn phone_style_1080p_level_4_1() {
    let mut c = Clip::new("h264_1080p.mp4", "high", "bframes=2:keyint=30:ref=3");
    c.size = "1920x1080";
    c.level = Some("4.1");
    check(c, 90);
}

/// A different encoder than x264 (NVENC, if this machine has it): B-frame pyramid, 8 references.
#[test]
fn nvenc_high_bframes_pyramid() {
    let mut c = Clip::new("h264_nvenc.mp4", "high", "");
    c.encoder = &["-c:v", "h264_nvenc", "-profile:v", "high", "-bf", "3", "-b_ref_mode", "middle", "-rc-lookahead", "8", "-refs", "4"];
    check(c, 90);
}

#[test]
fn frame_pictures_in_an_interlaced_capable_stream() {
    // frame_mbs_only_flag 0 without MBAFF: every picture is still a frame
    check(Clip::new("h264_fake_interlaced.mp4", "high", "fake-interlaced=1:bframes=2:keyint=30"), 90);
}

#[test]
fn mbaff_top_field_first() {
    check(Clip::new("h264_mbaff_tff.mp4", "high", "tff=1:bframes=2:keyint=30"), 90);
}

#[test]
fn mbaff_bottom_field_first() {
    check(Clip::new("h264_mbaff_bff.mp4", "high", "bff=1:bframes=2:keyint=30"), 90);
}

/// Field pictures (PAFF): x264 only writes MBAFF, NVENC with the interlaced flags writes every
/// picture as two fields. Skipped where there is no NVENC.
fn paff_clip(name: &'static str, field: &'static str, extra: &[&'static str]) -> Clip<'static> {
    let mut c = Clip::new(name, "high", "");
    c.vf = field;
    let mut enc = vec!["-c:v", "h264_nvenc", "-profile:v", "high", "-flags", "+ildct+ilme"];
    enc.extend_from_slice(extra);
    c.encoder = Box::leak(enc.into_boxed_slice());
    c
}

#[test]
fn paff_top_field_first() {
    check(paff_clip("h264_paff_tff.mp4", "setfield=tff", &["-bf", "0"]), 90);
}

#[test]
fn paff_bottom_field_first() {
    check(paff_clip("h264_paff_bff.mp4", "setfield=bff", &["-bf", "0"]), 90);
}

#[test]
fn paff_with_b_fields() {
    check(paff_clip("h264_paff_b.mp4", "setfield=tff", &["-bf", "3", "-b_ref_mode", "middle", "-refs", "4"]), 90);
}

/// The clip's stream copied into an MPEG transport stream (`.ts`, or `.m2ts` with the 4 byte
/// time stamps of AVCHD); `None` when ffmpeg cannot make it.
fn to_transport_stream(src: &Path, name: &str) -> Option<PathBuf> {
    if src.as_os_str().is_empty() {
        return None;
    }
    let out = temp_dir().join(name);
    if !out.exists() {
        let tmp = out.with_extension("part");
        let m2ts = name.ends_with(".m2ts");
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-y", "-v", "error", "-i"]).arg(src).args(["-c", "copy", "-f", "mpegts"]);
        if m2ts {
            cmd.args(["-mpegts_m2ts_mode", "1"]);
        }
        cmd.arg(&tmp);
        if !cmd.status().is_ok_and(|s| s.success()) {
            return None;
        }
        std::fs::rename(&tmp, &out).unwrap();
    }
    Some(out)
}

fn check_ts(c: Clip, name: &str, frames: usize) {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let src = make_clip(&c);
    let Some(ts) = to_transport_stream(&src, name) else {
        eprintln!("skipped: ffmpeg cannot encode {} here", c.name);
        return;
    };
    let Some(r) = compare(&ts, 100_000) else { return };
    eprintln!("{name}: {} frames compared with ffmpeg: {:?}", r.frames, r.first_mismatch);
    assert_eq!(r.frames, frames, "{name}: frame count");
    assert_eq!(r.first_mismatch, None, "{name}");
}

#[test]
fn interlaced_mbaff_in_a_transport_stream() {
    check_ts(Clip::new("h264_mbaff_tff.mp4", "high", "tff=1:bframes=2:keyint=30"), "h264_mbaff_tff.ts", 90);
}

#[test]
fn interlaced_paff_in_a_transport_stream() {
    check_ts(paff_clip("h264_paff_b.mp4", "setfield=tff", &["-bf", "3", "-b_ref_mode", "middle", "-refs", "4"]), "h264_paff_b.ts", 90);
}

/// AVCHD style: 1080i (1088 coded rows, cropped), 4 byte time stamped packets.
#[test]
fn interlaced_1080i_avchd_style_m2ts() {
    let mut c = Clip::new("h264_mbaff_1080i.mp4", "high", "tff=1:bframes=2:keyint=30:ref=3");
    c.size = "1920x1080";
    c.level = Some("4.1");
    check_ts(c, "h264_mbaff_1080i.m2ts", 90);
    let mut c = paff_clip("h264_paff_1080i.mp4", "setfield=tff", &["-bf", "2"]);
    c.size = "1920x1080";
    check_ts(c, "h264_paff_1080i.m2ts", 90);
}

/// Any clip named in RD_CLIP, for debugging (does nothing without it).
#[test]
fn clip_from_env() {
    let Some(p) = std::env::var_os("RD_CLIP") else { return };
    let Some(r) = compare(Path::new(&p), 100_000) else { return };
    eprintln!("RD_CLIP: {} frames, first mismatch {:?}, mismatching (frame, POC): {:?}", r.frames, r.first_mismatch, r.bad_frames);
}
