//! The linear field blend of the converter (`Converter::set_deinterlace`) on an interlaced
//! H.264 clip with real motion between the fields: the comb along moving edges shrinks, the
//! picture stays the picture.

#![cfg(windows)]

mod common;

use std::process::Command;

use common::*;
use rustdecoder::VideoDecoder;
use rustdecoder::present::{Colorimetry, Converter, Params, Source};

/// 60 fps moving test pattern, two source frames woven into each interlaced frame (top field
/// first), encoded with x264 (MBAFF).
fn interlaced_clip() -> std::path::PathBuf {
    let out = temp_dir().join("h264_woven_motion.mp4");
    if out.exists() {
        return out;
    }
    let tmp = out.with_extension("part.mp4");
    let st = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=60", "-t", "2", "-vf", "interlace=scan=tff", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-profile:v", "high"])
        .args(["-x264-params", "tff=1:bframes=2:keyint=30", "-r", "30"])
        .arg(&tmp)
        .status()
        .expect("ffmpeg");
    assert!(st.success());
    std::fs::rename(&tmp, &out).unwrap();
    out
}

/// Sum over rows of |row - average of the rows above and below| on the green channel: large
/// where two fields disagree (a comb), small on smooth and on blended pictures.
fn comb(rgba: &[u8], w: usize, h: usize) -> f64 {
    let mut sum = 0u64;
    for y in 1..h - 1 {
        for x in 0..w {
            let at = |yy: usize| rgba[(yy * w + x) * 4 + 1] as i32;
            sum += (at(y) - (at(y - 1) + at(y + 1)) / 2).unsigned_abs() as u64;
        }
    }
    sum as f64 / (w * (h - 2)) as f64
}

#[test]
fn blending_the_fields_removes_the_comb() {
    if !have_ffmpeg() {
        return eprintln!("skipped: ffmpeg not found");
    }
    let clip = interlaced_clip();
    let mut dec = match VideoDecoder::open(&clip) {
        Ok(d) => d,
        Err(rustdecoder::Error::Gpu(m)) => return eprintln!("skipped: {m}"),
        Err(e) => panic!("{e}"),
    };
    let info = dec.info().clone();
    let mut conv = Converter::new(&dec.gpu().device, &dec.gpu().context).unwrap();
    let params = Params { colour: Colorimetry::from_vui(&info.vui, info.width, info.height), rotation: 0, flip: false };
    let target = conv.create_target(info.width, info.height).unwrap();
    let (w, h) = (info.width as usize, info.height as usize);
    let (mut plain, mut blended, mut diff) = (0.0, 0.0, 0.0);
    let mut n = 0;
    for _ in 0..20 {
        let Some(f) = dec.next_frame().unwrap() else { break };
        conv.set_deinterlace(false);
        conv.convert(&Source::from_frame(&f), &params, &target).unwrap();
        let a = conv.read_rgba(&target).unwrap();
        conv.set_deinterlace(true);
        conv.convert(&Source::from_frame(&f), &params, &target).unwrap();
        let b = conv.read_rgba(&target).unwrap();
        plain += comb(&a, w, h);
        blended += comb(&b, w, h);
        diff += a.iter().zip(&b).map(|(x, y)| x.abs_diff(*y) as f64).sum::<f64>() / a.len() as f64;
        n += 1;
    }
    let k = n as f64;
    eprintln!("comb measure: woven {:.3}, blended {:.3}; mean abs change {:.2}", plain / k, blended / k, diff / k);
    assert!(n > 10);
    assert!(blended < plain, "the blend must reduce the comb");
    assert!(diff / k < 20.0, "the blend must not wreck the picture");
}
