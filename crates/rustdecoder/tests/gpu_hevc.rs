//! GPU decoding compared with ffmpeg's software decoder: HEVC decoding is exactly specified,
//! so every sample of every frame, in display order, has to match. Clips are generated with
//! ffmpeg + x265 into a temp folder (nothing is kept in git); tests skip with a message when
//! ffmpeg or a GPU decoder is missing.

#![cfg(windows)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rustdecoder::HevcDecoder;

const CAMERA_CLIP: &str = r"E:\gelen works 2026\dois erzincan\harbi dois\20261011_C5743.MP4";

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn temp_dir() -> PathBuf {
    let d = std::env::temp_dir().join("rustdecoder-tests");
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Makes (once) a 3 s clip of noisy test pattern, 640x360, with x265.
fn make_clip(name: &str, pix_fmt: &str, x265: &str) -> PathBuf {
    let out = temp_dir().join(name);
    if out.exists() {
        return out;
    }
    let tmp = out.with_extension("part.mp4");
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30", "-t", "3"])
        .args(["-vf", "noise=alls=6:allf=t"])
        .args(["-c:v", "libx265", "-pix_fmt", pix_fmt, "-x265-params"])
        .arg(format!("log-level=error:{x265}"))
        .arg(&tmp)
        .status()
        .expect("ffmpeg");
    assert!(status.success(), "ffmpeg could not make {name}");
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
    let mut dec = match HevcDecoder::open(clip) {
        Ok(d) => d,
        Err(rustdecoder::Error::Gpu(m)) => {
            eprintln!("skipped: {m}");
            return None;
        }
        Err(e) => panic!("open {}: {e}", clip.display()),
    };
    let info = dec.info().clone();
    let (pix, bytes) = if info.bit_depth > 8 { ("p010le", 2usize) } else { ("nv12", 1usize) };
    let mut ff = Command::new("ffmpeg")
        // the camera clip carries a rotation; frames are compared as coded
        .args(["-v", "error", "-noautorotate", "-i"])
        .arg(clip)
        .args(["-frames:v", &max.to_string(), "-f", "rawvideo", "-pix_fmt", pix, "-"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("ffmpeg");
    let mut out = ff.stdout.take().unwrap();
    let (w, h) = (info.width as usize, info.height as usize);
    let y_len = w * h * bytes;
    let uv_len = w * (h / 2) * bytes;
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
        let sample = |buf: &[u8], i: usize| -> u16 {
            if bytes == 2 { u16::from_le_bytes([buf[2 * i], buf[2 * i + 1]]) >> 6 } else { buf[i] as u16 }
        };
        {
            let (ry, ruv) = ref_frame.split_at(y_len);
            let bad = planes.y.iter().enumerate().find(|(i, v)| **v != sample(ry, *i)).map(|(i, v)| format!("Y x={} y={} gpu={} ffmpeg={}", i % w, i / w, v, sample(ry, i)));
            let bad = bad.or_else(|| {
                planes.uv.iter().enumerate().find(|(i, v)| **v != sample(ruv, *i)).map(|(i, v)| format!("UV x={} y={} gpu={} ffmpeg={}", i % w, i / w, v, sample(ruv, i)))
            });
            if let Some(b) = bad {
                if std::env::var_os("RD_DEBUG").is_some() {
                    let n_y = planes.y.iter().enumerate().filter(|(i, v)| **v != sample(ry, *i)).count();
                    let n_uv = planes.uv.iter().enumerate().filter(|(i, v)| **v != sample(ruv, *i)).count();
                    let mad = planes.y.iter().enumerate().map(|(i, v)| (*v as i64 - sample(ry, i) as i64).unsigned_abs()).sum::<u64>() as f64 / planes.y.len() as f64;
                    eprintln!("mean abs luma difference {mad:.2}");
                    eprintln!("frame {} POC {}: {n_y} of {} Y and {n_uv} of {} UV samples differ; first {b}", report.frames, f.poc, planes.y.len(), planes.uv.len());
                }
                report.bad_frames.push((report.frames, f.poc));
                report.first_mismatch.get_or_insert(format!("frame {} (POC {}): {b}", report.frames, f.poc));
            }
        }
        report.frames += 1;
    }
    report.fps = report.frames as f64 / t0.elapsed().as_secs_f64();
    if report.first_mismatch.is_none() && report.frames < max {
        // we ran out of frames: ffmpeg must not have more
        let mut extra = [0u8; 1];
        if out.read(&mut extra).unwrap_or(0) > 0 {
            report.first_mismatch = Some(format!("ffmpeg has more frames than the {} we decoded", report.frames));
        }
    }
    let _ = ff.kill();
    let _ = ff.wait();
    Some(report)
}

fn check(name: &str, pix_fmt: &str, x265: &str) {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let clip = make_clip(name, pix_fmt, x265);
    let Some(r) = compare(&clip, 1000) else { return };
    eprintln!("{name}: {} frames compared with ffmpeg ({:.0} fps incl. read-back): {:?}", r.frames, r.fps, r.first_mismatch);
    assert_eq!(r.frames, 90, "{name}: frame count");
    assert_eq!(r.first_mismatch, None, "{name}");
}

#[test]
fn main_8bit_bframes() {
    check("m8_b3.mp4", "yuv420p", "bframes=3:keyint=30:min-keyint=30");
}

#[test]
fn main10_bframes() {
    check("m10_b3.mp4", "yuv420p10le", "bframes=3:keyint=30:min-keyint=30");
}

#[test]
fn main10_open_gop_pyramid() {
    // open GOP: CRA pictures with leading pictures, deep B pyramid
    check("m10_open.mp4", "yuv420p10le", "bframes=6:b-pyramid=1:keyint=24:min-keyint=24:open-gop=1:ref=5");
}

#[test]
fn main_8bit_default_scaling_lists() {
    check("m8_sl.mp4", "yuv420p", "bframes=3:keyint=30:scaling-list=default");
}

#[test]
fn main10_slices_and_tiles_off() {
    check("m10_slices.mp4", "yuv420p10le", "bframes=2:keyint=30:slices=4");
}

#[test]
fn main_8bit_no_bframes() {
    check("m8_p.mp4", "yuv420p", "bframes=0:keyint=20:ref=3");
}

/// Any clip named in RD_CLIP, for debugging (does nothing without it).
#[test]
fn clip_from_env() {
    let Some(p) = std::env::var_os("RD_CLIP") else { return };
    let Some(r) = compare(Path::new(&p), 1000) else { return };
    eprintln!("RD_CLIP: {} frames, first mismatch {:?}, mismatching (frame, POC): {:?}", r.frames, r.first_mismatch, r.bad_frames);
}

/// The first 60 frames of a Sony XAVC HS 4K 59.94 fps Main 10 clip (read only, if present).
#[test]
fn camera_clip_first_60_frames() {
    let clip = Path::new(CAMERA_CLIP);
    if !clip.exists() || !have_ffmpeg() {
        eprintln!("skipped: camera clip or ffmpeg not found");
        return;
    }
    let Some(r) = compare(clip, 60) else { return };
    eprintln!("camera clip: {} frames compared with ffmpeg: {:?}; mismatching (frame, POC): {:?}", r.frames, r.first_mismatch, r.bad_frames);
    assert_eq!(r.frames, 60);
    assert_eq!(r.first_mismatch, None);
}
