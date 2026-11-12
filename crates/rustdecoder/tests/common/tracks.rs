//! Clips with several audio tracks for the track tests, made with ffmpeg.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

use super::temp_dir;

static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A 2 s clip (x264 video at 25 fps) with one audio stream per entry of `freqs` (a sine of that
/// frequency each, 48 kHz mono), encoded with `acodec`; `extra` are more output arguments
/// (metadata, dispositions) and the container follows the extension of `name`.
pub fn make(name: &str, freqs: &[u32], acodec: &[&str], extra: &[&str]) -> PathBuf {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let out = temp_dir().join(name);
    if out.exists() {
        return out;
    }
    let tmp = out.with_file_name(format!("part-{name}"));
    let mut c = Command::new("ffmpeg");
    c.args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=25"]);
    for f in freqs {
        c.args(["-f", "lavfi", "-i"]).arg(format!("sine=f={f}:r=48000:d=2"));
    }
    c.args(["-map", "0:v"]);
    for i in 0..freqs.len() {
        c.args(["-map", &format!("{}:a", i + 1)]);
    }
    c.args(["-t", "2", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-preset", "ultrafast", "-g", "12"]);
    c.args(acodec).args(extra);
    if name.ends_with(".mxf") {
        c.args(["-f", "mxf"]);
    }
    let st = c.arg(&tmp).status().expect("ffmpeg");
    assert!(st.success(), "ffmpeg could not make {name}");
    std::fs::rename(&tmp, &out).unwrap();
    out
}

pub const PCM24: &[&str] = &["-c:a", "pcm_s24le"];

/// Language clip: two tracks, Turkish first, English marked default.
pub fn two_languages(ext: &str, acodec: &[&str]) -> PathBuf {
    make(
        &format!("tracks_two_lang.{ext}"),
        &[440, 880],
        acodec,
        &[
            "-metadata:s:a:0", "language=tur", "-metadata:s:a:1", "language=eng", "-metadata:s:a:0", "title=Turkce", "-metadata:s:a:1", "title=English",
            "-metadata:s:a:0", "handler_name=Turkce", "-metadata:s:a:1", "handler_name=English", "-disposition:a:1", "default",
        ],
    )
}

/// Frequency of the first channel from rising zero crossings.
pub fn zero_cross_freq(x: &[f32], ch: usize, rate: u32) -> f64 {
    let mono: Vec<f32> = x.chunks_exact(ch).map(|f| f[0]).collect();
    let (a, b) = (mono.len() / 10, mono.len() * 9 / 10);
    let (mut first, mut last, mut n) = (None, 0.0, 0);
    for i in a..b - 1 {
        if mono[i] < 0.0 && mono[i + 1] >= 0.0 {
            let t = i as f64 + (-mono[i] as f64) / (mono[i + 1] - mono[i]) as f64;
            first.get_or_insert(t);
            last = t;
            n += 1;
        }
    }
    (n - 1) as f64 * rate as f64 / (last - first.unwrap())
}

