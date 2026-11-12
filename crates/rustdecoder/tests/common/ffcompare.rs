//! Decoded pictures against ffmpeg's software decoder.
#![cfg(windows)]
#![allow(dead_code)]

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use rustdecoder::VideoDecoder;

/// Decodes up to `max` frames of `clip` on the GPU and compares them with ffmpeg's NV12 output
/// (8-bit 4:2:0). Returns the frames compared and the first difference; `None` when this machine
/// has no decoder for the clip.
pub fn compare_decode(clip: &Path, max: usize) -> Option<(usize, Option<String>)> {
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
