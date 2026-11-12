//! Matroska / WebM clips made with ffmpeg, and ffprobe's packet list to compare against.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use super::temp_dir;

/// Makes (once) a 3 s Matroska / WebM clip: 640x360 test pattern at 30 fps with a sine tone.
/// `video` and `audio` are ffmpeg encoder arguments, `mux` extra muxer arguments; `name`'s
/// extension picks mkv or webm.
pub fn make_mkv(name: &str, video: &[&str], audio: &[&str], mux: &[&str]) -> PathBuf {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let out = temp_dir().join(name);
    if out.exists() {
        return out;
    }
    let ext = out.extension().unwrap().to_str().unwrap().to_string();
    let tmp = out.with_file_name(format!("part-{name}"));
    let mut c = Command::new("ffmpeg");
    c.args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30", "-f", "lavfi", "-i", "sine=f=440:r=48000:d=3", "-t", "3"]);
    c.args(video).args(audio).args(mux).args(["-f", if ext == "webm" { "webm" } else { "matroska" }]).arg(&tmp);
    let status = c.status().expect("ffmpeg");
    assert!(status.success(), "ffmpeg could not make {name}");
    std::fs::rename(&tmp, &out).unwrap();
    out
}

/// One packet as ffprobe lists it.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Packet {
    pub pts: i64,
    pub size: u32,
    pub key: bool,
}

/// `ffprobe -show_packets` of one stream (`v:0`, `a:0`), in file order.
pub fn ffprobe_packets(clip: &Path, stream: &str) -> Vec<Packet> {
    let o = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", stream, "-show_entries", "packet=pts,size,flags", "-of", "compact"])
        .arg(clip)
        .output()
        .expect("ffprobe");
    assert!(o.status.success(), "ffprobe failed");
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter(|l| l.starts_with("packet"))
        .map(|l| {
            let get = |k: &str| l.split('|').find_map(|f| f.strip_prefix(&format!("{k}="))).unwrap_or("");
            Packet { pts: get("pts").parse().unwrap_or(i64::MIN), size: get("size").parse().unwrap(), key: get("flags").starts_with('K') }
        })
        .collect()
}
