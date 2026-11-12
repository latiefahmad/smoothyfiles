//! Fragmented MP4 clips made with ffmpeg (shared by the reader, decode and playback tests).
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

use super::temp_dir;

pub const FRAG_H264: &[&str] = &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-x264-params", "log-level=error:keyint=15:min-keyint=15:bframes=3:scenecut=0"];
pub const FRAG_HEVC: &[&str] = &["-c:v", "libx265", "-pix_fmt", "yuv420p", "-x265-params", "log-level=error:keyint=15:min-keyint=15"];

/// Makes (once) a 3 s H.264 / HEVC + AAC clip (mono, 48 kHz) with the given muxer arguments.
pub fn make_frag(name: &str, video: &[&str], mux: &[&str]) -> PathBuf {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let out = temp_dir().join(name);
    if out.exists() {
        return out;
    }
    let tmp = out.with_file_name(format!("part-{name}"));
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30", "-f", "lavfi", "-i", "sine=f=440:r=48000:d=3", "-t", "3", "-vf", "noise=alls=6:allf=t"])
        .args(video)
        .args(["-c:a", "aac", "-b:a", "96k"])
        .args(mux)
        .args(["-f", "mp4"])
        .arg(&tmp)
        .status()
        .expect("ffmpeg");
    assert!(status.success(), "ffmpeg could not make {name}");
    std::fs::rename(&tmp, &out).unwrap();
    out
}
