//! Helpers shared by the playback tests: clips made with ffmpeg + x265 into a temp folder.
#![allow(dead_code)]

pub mod av;
pub mod contclips;
#[cfg(windows)]
pub mod ffcompare;
pub mod fragclips;
pub mod mkvclips;
pub mod tracks;
#[allow(unused_imports)]
pub use fragclips::*;
pub use contclips::*;
pub use mkvclips::*;

use std::path::PathBuf;
use std::process::{Command, Stdio};

pub const CAMERA_CLIP: &str = r"E:\gelen works 2026\dois erzincan\harbi dois\20261011_C5743.MP4";

pub fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

pub fn temp_dir() -> PathBuf {
    let d = std::env::temp_dir().join("rustdecoder-tests");
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Makes (once) a 3 s clip of 640x360 test pattern at 30 fps with x265. `noise` adds film grain
/// like the decode tests do; without it the picture is clean, which suits colour comparisons.
pub fn make_clip(name: &str, pix_fmt: &str, x265: &str, noise: bool) -> PathBuf {
    let out = temp_dir().join(name);
    if out.exists() {
        return out;
    }
    let tmp = out.with_extension("part.mp4");
    let mut c = Command::new("ffmpeg");
    c.args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30", "-t", "3"]);
    if noise {
        c.args(["-vf", "noise=alls=6:allf=t"]);
    }
    let status = c
        .args(["-c:v", "libx265", "-pix_fmt", pix_fmt, "-x265-params"])
        .arg(format!("log-level=error:{x265}"))
        .arg(&tmp)
        .status()
        .expect("ffmpeg");
    assert!(status.success(), "ffmpeg could not make {name}");
    std::fs::rename(&tmp, &out).unwrap();
    out
}

/// Remuxes `clip` with a display rotation (ffmpeg's degrees are counter-clockwise).
pub fn make_rotated(clip: &std::path::Path, name: &str, ccw_degrees: i32) -> PathBuf {
    let out = temp_dir().join(name);
    if out.exists() {
        return out;
    }
    let tmp = out.with_extension("part.mp4");
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-display_rotation", &ccw_degrees.to_string(), "-i"])
        .arg(clip)
        .args(["-c", "copy"])
        .arg(&tmp)
        .status()
        .expect("ffmpeg");
    assert!(status.success(), "ffmpeg could not rotate {name}");
    std::fs::rename(&tmp, &out).unwrap();
    out
}

/// Makes (once) a 3 s clip of 640x360 test pattern at 30 fps with x264 (8-bit 4:2:0).
pub fn make_h264_clip(name: &str, x264: &str, noise: bool) -> PathBuf {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let out = temp_dir().join(name);
    if out.exists() {
        return out;
    }
    let tmp = out.with_extension("part.mp4");
    let mut c = Command::new("ffmpeg");
    c.args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30", "-t", "3"]);
    if noise {
        c.args(["-vf", "noise=alls=6:allf=t"]);
    }
    let status = c.args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-x264-params"]).arg(format!("log-level=error:{x264}")).arg(&tmp).status().expect("ffmpeg");
    assert!(status.success(), "ffmpeg could not make {name}");
    std::fs::rename(&tmp, &out).unwrap();
    out
}
