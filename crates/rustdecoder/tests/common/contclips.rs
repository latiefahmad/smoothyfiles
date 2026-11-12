//! Clips for the transport stream / AVI / program stream readers, and ffprobe's full packet list.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use super::temp_dir;

/// Makes (once) a 3 s clip of 640x360 test pattern at 30 fps with a 440 Hz tone. `video` and
/// `audio` are encoder arguments, `mux` the muxer's (`-f ...` included); `name`'s extension is
/// only a name. Pass an empty `audio` to get video only (`-an`).
pub fn make_av(name: &str, video: &[&str], audio: &[&str], mux: &[&str]) -> PathBuf {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let out = temp_dir().join(name);
    if out.exists() {
        return out;
    }
    let tmp = out.with_file_name(format!("part-{name}"));
    let mut c = Command::new("ffmpeg");
    c.args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30", "-f", "lavfi", "-i", "sine=f=440:r=48000:d=3", "-t", "3"]);
    c.args(video);
    if audio.is_empty() {
        c.arg("-an");
    } else {
        c.args(audio);
    }
    c.args(mux).arg(&tmp);
    let status = c.status().expect("ffmpeg");
    assert!(status.success(), "ffmpeg could not make {name}");
    std::fs::rename(&tmp, &out).unwrap();
    out
}

/// One packet as ffprobe lists it, with the CRC32 it computes over the packet's data.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Pkt {
    pub pts: i64,
    pub dts: i64,
    pub size: u32,
    pub key: bool,
    pub crc: u32,
}

/// `ffprobe -show_packets -show_data_hash CRC32` of one stream (`v:0`, `a:0`), in file order.
/// Missing times are `i64::MIN`.
pub fn ffprobe_full(clip: &Path, stream: &str) -> Vec<Pkt> {
    let o = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", stream, "-show_entries", "packet=pts,dts,size,flags,data_hash", "-show_data_hash", "CRC32", "-of", "compact"])
        .arg(clip)
        .output()
        .expect("ffprobe");
    assert!(o.status.success(), "ffprobe failed");
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter(|l| l.starts_with("packet"))
        .map(|l| {
            let get = |k: &str| l.split('|').find_map(|f| f.strip_prefix(&format!("{k}="))).unwrap_or("");
            Pkt {
                pts: get("pts").parse().unwrap_or(i64::MIN),
                dts: get("dts").parse().unwrap_or(i64::MIN),
                size: get("size").parse().unwrap(),
                key: get("flags").starts_with('K'),
                crc: u32::from_str_radix(get("data_hash").trim_start_matches("CRC32:"), 16).unwrap_or(0),
            }
        })
        .collect()
}
