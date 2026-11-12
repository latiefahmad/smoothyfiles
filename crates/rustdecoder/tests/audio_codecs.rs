//! Opus, FLAC, MP3, AC-3, E-AC-3 and Vorbis through the OS decoders, against ffmpeg: decoded
//! PCM (`ffmpeg -f f32le` of the same track), the time of the first sample and the length.
//! FLAC must be bit-exact, lossy codecs are within a tolerance. Run with `--nocapture` for the
//! figures. A test skips (and says so) when ffmpeg, or the OS decoder for its codec, is missing.

#![cfg(windows)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::av::*;
use common::{have_ffmpeg, temp_dir};
use rustdecoder::audio::AudioDecoder;

struct Diff {
    max: f32,
    rms: f64,
}

fn diff(a: &[f32], b: &[f32]) -> Diff {
    let n = a.len().min(b.len());
    let (mut max, mut sum) = (0f32, 0f64);
    for i in 0..n {
        let d = (a[i] - b[i]).abs();
        max = max.max(d);
        sum += (d as f64) * (d as f64);
    }
    Diff { max, rms: (sum / n.max(1) as f64).sqrt() }
}

/// Shift (in frames, positive: ours is late) of `ours` against `theirs` that makes channel 0
/// match best, searched within +-`range` frames over a window in the middle.
fn best_shift(ours: &[f32], theirs: &[f32], ch: usize, range: i64) -> i64 {
    let x: Vec<f32> = ours.iter().step_by(ch).copied().collect();
    let y: Vec<f32> = theirs.iter().step_by(ch).copied().collect();
    let (a, b) = (x.len().min(y.len()) / 2, x.len().min(y.len()) / 2 + 4000);
    let mut best = (f64::MAX, 0i64);
    // smallest shifts first, and only a clearly better one replaces: a sine matches at every
    // multiple of its period
    let mut order: Vec<i64> = (-range..=range).collect();
    order.sort_by_key(|s| s.abs());
    for s in order {
        let mut e = 0f64;
        for i in a..b {
            let j = i as i64 - s;
            if j < 0 || j as usize >= y.len() {
                continue;
            }
            let d = (x[i] - y[j as usize]) as f64;
            e += d * d;
        }
        if e < best.0 * 0.5 {
            best = (e, s);
        }
    }
    best.1
}

/// Makes (once) a 3 s clip, 640x360 H.264, with a sine track: 440 Hz left / 660 Hz right in
/// stereo, or six different tones (5.1).
fn make(name: &str, rate: u32, channels: u32, acodec: &[&str]) -> PathBuf {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let out = temp_dir().join(name);
    if out.exists() {
        return out;
    }
    let tmp = out.with_file_name(format!("part-{name}"));
    let mut c = Command::new("ffmpeg");
    c.args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30"]);
    match channels {
        6 => {
            c.args(["-f", "lavfi", "-i"]).arg(format!("aevalsrc=0.5*sin(2*PI*220*t)|0.5*sin(2*PI*330*t)|0.5*sin(2*PI*440*t)|0.3*sin(2*PI*100*t)|0.5*sin(2*PI*550*t)|0.5*sin(2*PI*660*t):s={rate}:d=3:c=5.1"));
        }
        2 => {
            c.args(["-f", "lavfi", "-i"]).arg(format!("sine=f=440:r={rate}:d=3"));
            c.args(["-f", "lavfi", "-i"]).arg(format!("sine=f=660:r={rate}:d=3"));
            c.args(["-filter_complex", "[1:a][2:a]amerge=inputs=2[a]", "-map", "0:v", "-map", "[a]"]);
        }
        _ => {
            c.args(["-f", "lavfi", "-i"]).arg(format!("sine=f=440:r={rate}:d=3"));
        }
    }
    let video: &[&str] = if name.ends_with(".webm") {
        &["-c:v", "libvpx-vp9", "-pix_fmt", "yuv420p", "-b:v", "500k", "-deadline", "realtime", "-cpu-used", "8"]
    } else {
        &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-preset", "veryfast"]
    };
    let ok = c.args(["-t", "3"]).args(video).args(acodec).arg(&tmp).status().expect("ffmpeg").success();
    assert!(ok, "ffmpeg could not make {name}");
    std::fs::rename(&tmp, &out).unwrap();
    out
}

struct Result_ {
    d: Diff,
    ours: usize,
    theirs: usize,
    first: f64,
    shift: i64,
    ch: usize,
    rate: u32,
}

/// Decodes `clip` both ways. `None` (skipped) when no decoder is installed for the codec.
fn compare(clip: &Path, label: &str) -> Option<Result_> {
    let mut dec = match AudioDecoder::open(clip) {
        Ok(Some(d)) => d,
        Ok(None) => panic!("no audio track in {clip:?}"),
        Err(rustdecoder::Error::Unsupported(m)) if m.contains("no ") && m.contains("decoder installed") => {
            eprintln!("skipped {label}: {m}");
            return None;
        }
        Err(e) => panic!("{label}: open: {e}"),
    };
    let info = dec.info().clone();
    let (first, ours) = dec.decode_all().unwrap();
    let theirs = ffmpeg_audio(clip);
    let ch = info.channels as usize;
    let start = ffprobe_audio_start(clip);
    let n = ours.len().min(theirs.len());
    let shift = best_shift(&ours, &theirs, ch, 3000);
    // ours[i] is theirs[i - shift]
    let off = shift.unsigned_abs() as usize * ch;
    let d = if shift >= 0 { diff(&ours[off..n], &theirs[..n - off]) } else { diff(&ours[..n - off], &theirs[off..n]) };
    println!(
        "{label}: {} {} Hz {ch} ch; ours {} frames from {first:.6} s, ffmpeg {} frames (start {start:.6} s); max abs diff {:.3e}, rms {:.3e}; best alignment shift {shift} frames",
        info.codec,
        info.sample_rate,
        ours.len() / ch,
        theirs.len() / ch,
        d.max,
        d.rms
    );
    Some(Result_ { d, ours: ours.len() / ch, theirs: theirs.len() / ch, first, shift, ch, rate: info.sample_rate })
}

/// `shift`: how many frames the OS decoder's output is ahead of ffmpeg's (its own delay
/// compensation, see the design document), 0 when they agree.
fn lossy(name: &str, rate: u32, channels: u32, acodec: &[&str], max: f32, rms: f64, len_tol: usize, shift: i64) {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let clip = make(name, rate, channels, acodec);
    let Some(r) = compare(&clip, name) else { return };
    assert_eq!(r.rate, rate);
    assert_eq!(r.ch, channels as usize);
    assert!(r.first.abs() < 1.0 / rate as f64, "first sample at {}", r.first);
    assert!(r.ours.abs_diff(r.theirs) <= len_tol, "length {} vs {}", r.ours, r.theirs);
    assert_eq!(r.shift, shift, "decoded audio is shifted by {} frames", r.shift);
    assert!(r.d.max < max, "max diff {}", r.d.max);
    assert!(r.d.rms < rms, "rms {}", r.d.rms);
}

const OPUS: &[&str] = &["-c:a", "libopus", "-b:a", "96k"];
const FLAC: &[&str] = &["-c:a", "flac"];
const MP3: &[&str] = &["-c:a", "libmp3lame", "-b:a", "128k"];
const AC3: &[&str] = &["-c:a", "ac3", "-b:a", "192k"];
const AC3_51: &[&str] = &["-c:a", "ac3", "-b:a", "448k"];
const EAC3: &[&str] = &["-c:a", "eac3", "-b:a", "192k"];
const VORBIS: &[&str] = &["-c:a", "libvorbis", "-q:a", "4"];

#[test]
fn opus_mkv() {
    lossy("cd_opus.mkv", 48000, 2, OPUS, 1e-4, 1e-5, 0, 0);
}

#[test]
fn opus_webm() {
    lossy("cd_opus.webm", 48000, 2, OPUS, 1e-4, 1e-5, 0, 0);
}

#[test]
fn opus_mp4() {
    lossy("cd_opus.mp4", 48000, 2, OPUS, 1e-4, 1e-5, 0, 0);
}

#[test]
fn flac_mkv_is_bit_exact() {
    if !have_ffmpeg() {
        return;
    }
    let clip = make("cd_flac.mkv", 48000, 2, FLAC);
    let Some(r) = compare(&clip, "flac mkv") else { return };
    assert_eq!((r.ours, r.d.max), (r.theirs, 0.0));
    assert_eq!(r.first, 0.0);
}

#[test]
fn flac_mp4_is_bit_exact() {
    if !have_ffmpeg() {
        return;
    }
    let clip = make("cd_flac.mp4", 48000, 2, &["-c:a", "flac", "-strict", "-2"]);
    let Some(r) = compare(&clip, "flac mp4") else { return };
    assert_eq!((r.ours, r.d.max), (r.theirs, 0.0));
    assert_eq!(r.first, 0.0);
}

#[test]
fn flac_44k_mono_is_bit_exact() {
    if !have_ffmpeg() {
        return;
    }
    let clip = make("cd_flac_mono44.mkv", 44100, 1, FLAC);
    let Some(r) = compare(&clip, "flac mono 44.1k") else { return };
    assert_eq!((r.ours, r.d.max), (r.theirs, 0.0));
}

#[test]
fn mp3_mkv() {
    lossy("cd_mp3.mkv", 48000, 2, MP3, 1e-4, 5e-5, 1152, -528);
}

#[test]
fn mp3_mp4() {
    lossy("cd_mp3.mp4", 48000, 2, MP3, 1e-4, 5e-5, 1152, -528);
}

#[test]
fn ac3_mkv() {
    lossy("cd_ac3.mkv", 48000, 2, AC3, 0.01, 1e-3, 1536, 0);
}

#[test]
fn ac3_mp4() {
    lossy("cd_ac3.mp4", 48000, 2, AC3, 0.01, 1e-3, 1536, 0);
}

#[test]
fn ac3_51_mkv() {
    lossy("cd_ac3_51.mkv", 48000, 6, AC3_51, 0.01, 1e-3, 1536, 0);
}

#[test]
fn eac3_mkv() {
    lossy("cd_eac3.mkv", 48000, 2, EAC3, 0.01, 1e-3, 1536, 0);
}

#[test]
fn eac3_mp4() {
    lossy("cd_eac3.mp4", 48000, 2, EAC3, 0.01, 1e-3, 1536, 0);
}

#[test]
fn vorbis_mkv() {
    lossy("cd_vorbis.mkv", 48000, 2, VORBIS, 1e-5, 1e-6, 1024, -128);
}

#[test]
fn vorbis_webm() {
    lossy("cd_vorbis.webm", 48000, 2, VORBIS, 1e-5, 1e-6, 1024, -128);
}
