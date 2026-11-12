//! Fragmented MP4 through the GPU decoders: the pictures have to match ffmpeg's software
//! decoder sample for sample, in display order (the reader's sample table is all the decoders
//! see, so this proves the table, not the decoders).

#![cfg(windows)]
mod common;

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use common::*;
use rustdecoder::VideoDecoder;

/// Decodes up to `max` frames and compares them with ffmpeg's NV12 output. Returns the frames
/// compared and the first difference; `None` when this machine has no decoder for the clip.
fn compare(clip: &Path, max: usize) -> Option<(usize, Option<String>)> {
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
        .args(["-fps_mode", "passthrough", "-frames:v", &max.to_string(), "-f", "rawvideo", "-pix_fmt", "nv12", "-"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("ffmpeg");
    let mut out = ff.stdout.take().unwrap();
    let (w, h) = (info.width as usize, info.height as usize);
    let y_len = w * h;
    let mut ref_frame = vec![0u8; y_len + w * (h / 2)];
    let (mut frames, mut first) = (0usize, None);
    while frames < max {
        let Some(f) = dec.next_frame().unwrap() else { break };
        let planes = dec.read_back(&f).unwrap();
        if out.read_exact(&mut ref_frame).is_err() {
            first = Some(format!("ffmpeg has fewer frames than we decoded (we are at {frames})"));
            break;
        }
        let (ry, ruv) = ref_frame.split_at(y_len);
        let bad = planes.y.iter().zip(ry).position(|(a, b)| *a != *b as u16).map(|i| format!("Y {i}"));
        let bad = bad.or_else(|| planes.uv.iter().zip(ruv).position(|(a, b)| *a != *b as u16).map(|i| format!("UV {i}")));
        if let Some(b) = bad {
            first.get_or_insert(format!("frame {frames} (POC {}): {b}", f.poc));
        }
        frames += 1;
    }
    let _ = ff.kill();
    let _ = ff.wait();
    Some((frames, first))
}

fn check(name: &str, video: &[&str], mux: &[&str], frames: usize) {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let clip = make_frag(name, video, mux);
    let Some((n, first)) = compare(&clip, 100_000) else { return };
    eprintln!("{name}: {n} frames compared with ffmpeg: {first:?}");
    assert_eq!(n, frames, "{name}: frame count");
    assert_eq!(first, None, "{name}");
}

#[test]
fn h264_empty_moov() {
    check("frag_h264_kf.mp4", FRAG_H264, &["-movflags", "frag_keyframe+empty_moov"], 90);
}

#[test]
fn h264_mixed() {
    check("frag_h264_mixed.mp4", FRAG_H264, &["-movflags", "frag_keyframe"], 90);
}

#[test]
fn h264_cmaf_negative_offsets() {
    check("frag_h264_cmaf.mp4", FRAG_H264, &["-movflags", "+cmaf"], 90);
}

#[test]
fn h264_one_fragment_per_frame() {
    check("frag_h264_perframe.mp4", FRAG_H264, &["-movflags", "empty_moov+default_base_moof", "-frag_duration", "1"], 90);
}

#[test]
fn hevc_empty_moov() {
    check("frag_hevc_kf.mp4", FRAG_HEVC, &["-movflags", "frag_keyframe+empty_moov"], 90);
}

#[test]
fn hevc_dash_separate_moof() {
    check("frag_hevc_sep.mp4", FRAG_HEVC, &["-movflags", "frag_keyframe+empty_moov+separate_moof"], 90);
}

#[test]
fn hevc_cmaf_negative_offsets() {
    check("frag_hevc_cmaf.mp4", FRAG_HEVC, &["-movflags", "+cmaf"], 90);
}

/// A recording that was cut in its last `mdat`: the whole pictures decode, bit-exact.
#[test]
fn cut_recording_decodes_what_is_whole() {
    if !have_ffmpeg() {
        return;
    }
    let clip = make_frag("frag_h264_kf.mp4", FRAG_H264, &["-movflags", "frag_keyframe+empty_moov"]);
    let b = std::fs::read(&clip).unwrap();
    // the last mdat starts after the last moof
    let moof = b.windows(4).rposition(|w| w == b"moof").unwrap() - 4;
    let size = u32::from_be_bytes(b[moof..moof + 4].try_into().unwrap()) as usize;
    let mdat = moof + size;
    assert_eq!(&b[mdat + 4..mdat + 8], b"mdat");
    let cut = temp_dir().join("frag_h264_kf_cut_decode.mp4");
    std::fs::write(&cut, &b[..mdat + 8 + (b.len() - mdat - 8) / 2]).unwrap();
    let movie = rustdecoder::container::read(&mut std::fs::File::open(&cut).unwrap()).unwrap();
    let n = movie.video().unwrap().samples.len();
    let Some((frames, first)) = compare(&cut, n) else { return };
    eprintln!("cut file: {frames} of {n} pictures compared with ffmpeg: {first:?}");
    assert_eq!(frames, n);
    assert_eq!(first, None);
}


/// Control: the same streams remuxed into an ordinary MP4 compare the same way.
#[test]
fn control_plain_remux() {
    if !have_ffmpeg() {
        return;
    }
    let plain = make_frag("frag_h264_plain_control.mp4", FRAG_H264, &[]);
    let Some((n, first)) = compare(&plain, 100_000) else { return };
    eprintln!("plain remux: {n} frames, {first:?}");
    assert_eq!(first, None);
}
