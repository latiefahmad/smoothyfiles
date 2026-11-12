//! YUV → RGB on the GPU compared with ffmpeg's own conversion of the same pictures. Clips are
//! made with ffmpeg + x265 into a temp folder; tests skip with a message when ffmpeg or a GPU
//! decoder is missing.

#![cfg(windows)]

mod common;

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use common::*;
use rustdecoder::HevcDecoder;
use rustdecoder::present::{Colorimetry, Converter, Params, Source, rotated_size};

struct Err3 {
    mae: [f64; 3],
    max: [u8; 3],
    frames: usize,
    size: (u32, u32),
}

fn open(clip: &Path) -> Option<HevcDecoder> {
    match HevcDecoder::open(clip) {
        Ok(d) => Some(d),
        Err(rustdecoder::Error::Gpu(m)) => {
            eprintln!("skipped: {m}");
            None
        }
        Err(e) => panic!("open: {e}"),
    }
}

/// Converts the first `frames` pictures with our shader and with ffmpeg (`vf` is the scale
/// filter with the input matrix and range spelled out) and measures the difference.
fn compare(clip: &Path, vf: &str, frames: usize, tweak: impl Fn(&mut Params)) -> Option<Err3> {
    let mut dec = open(clip)?;
    let info = dec.info().clone();
    let mut conv = Converter::new(&dec.gpu().device, &dec.gpu().context).expect("converter");
    let mut params = Params { colour: Colorimetry::from_vui(&info.vui, info.width, info.height), rotation: info.rotation, flip: false };
    tweak(&mut params);
    let (ow, oh) = rotated_size(info.width, info.height, params.rotation);
    let target = conv.create_target(ow, oh).unwrap();
    let mut ff = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(clip)
        .args(["-frames:v", &frames.to_string(), "-vf", vf, "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("ffmpeg");
    let mut out = ff.stdout.take().unwrap();
    let mut refbuf = vec![0u8; (ow * oh * 3) as usize];
    let mut sum = [0u64; 3];
    let mut max = [0u8; 3];
    let mut n = 0usize;
    while n < frames {
        let Some(f) = dec.next_frame().unwrap() else { break };
        conv.convert(&Source::from_frame(&f), &params, &target).unwrap();
        let ours = conv.read_rgba(&target).unwrap();
        out.read_exact(&mut refbuf).expect("ffmpeg frame");
        for (a, b) in ours.chunks_exact(4).zip(refbuf.chunks_exact(3)) {
            for c in 0..3 {
                let d = a[c].abs_diff(b[c]);
                sum[c] += d as u64;
                max[c] = max[c].max(d);
            }
        }
        n += 1;
    }
    let _ = ff.kill();
    let _ = ff.wait();
    let px = (n as u64 * ow as u64 * oh as u64) as f64;
    Some(Err3 { mae: [sum[0] as f64 / px, sum[1] as f64 / px, sum[2] as f64 / px], max, frames: n, size: (ow, oh) })
}

const FLAGS: &str = "flags=bilinear+accurate_rnd+full_chroma_int";
const TAG709: &str = "bframes=3:keyint=30:min-keyint=30:colorprim=bt709:transfer=bt709:colormatrix=bt709:range=limited";

fn report(name: &str, e: &Err3) {
    eprintln!("{name}: {} frames {}x{}: mean abs error R {:.3} G {:.3} B {:.3} (max {:?})", e.frames, e.size.0, e.size.1, e.mae[0], e.mae[1], e.mae[2], e.max);
}

#[test]
fn bt709_8bit_matches_ffmpeg() {
    if !have_ffmpeg() {
        return eprintln!("skipped: ffmpeg not found");
    }
    let clip = make_clip("c709_8.mp4", "yuv420p", TAG709, false);
    let vf = format!("scale=in_color_matrix=bt709:in_range=tv:out_range=full:{FLAGS}");
    let Some(e) = compare(&clip, &vf, 12, |_| {}) else { return };
    report("BT.709 8-bit", &e);
    assert!(e.mae.iter().all(|m| *m < 2.0), "{:?}", e.mae);
}

#[test]
fn bt709_10bit_matches_ffmpeg() {
    if !have_ffmpeg() {
        return eprintln!("skipped: ffmpeg not found");
    }
    let clip = make_clip("c709_10.mp4", "yuv420p10le", TAG709, false);
    let vf = format!("scale=in_color_matrix=bt709:in_range=tv:out_range=full:{FLAGS}");
    let Some(e) = compare(&clip, &vf, 12, |_| {}) else { return };
    report("BT.709 10-bit", &e);
    assert!(e.mae.iter().all(|m| *m < 2.0), "{:?}", e.mae);
}

#[test]
fn untagged_sd_is_bt601() {
    if !have_ffmpeg() {
        return eprintln!("skipped: ffmpeg not found");
    }
    // 640x360 without VUI: below HD, so BT.601
    let clip = make_clip("c_untagged.mp4", "yuv420p", "bframes=3:keyint=30:min-keyint=30", false);
    let vf = format!("scale=in_color_matrix=bt601:in_range=tv:out_range=full:{FLAGS}");
    let Some(e) = compare(&clip, &vf, 12, |_| {}) else { return };
    report("untagged 640x360 (BT.601)", &e);
    assert!(e.mae.iter().all(|m| *m < 2.0), "{:?}", e.mae);
}

#[test]
fn bt2020_full_range_10bit_matches_ffmpeg() {
    if !have_ffmpeg() {
        return eprintln!("skipped: ffmpeg not found");
    }
    // SDR transfer with the BT.2020 matrix, full range: matrix and range paths
    let clip = make_clip("c2020_10_full.mp4", "yuv420p10le", "bframes=3:keyint=30:min-keyint=30:colorprim=bt2020:transfer=bt709:colormatrix=bt2020nc:range=full", false);
    let vf = format!("scale=in_color_matrix=bt2020:in_range=pc:out_range=full:{FLAGS}");
    let Some(e) = compare(&clip, &vf, 12, |p| p.colour.bt2020_primaries = false) else { return };
    report("BT.2020 full range 10-bit", &e);
    assert!(e.mae.iter().all(|m| *m < 2.0), "{:?}", e.mae);
}

#[test]
fn rotation_matches_ffmpeg_autorotate() {
    if !have_ffmpeg() {
        return eprintln!("skipped: ffmpeg not found");
    }
    let base = make_clip("c709_8.mp4", "yuv420p", TAG709, false);
    for ccw in [90, 180, 270] {
        let clip = make_rotated(&base, &format!("c709_8_rot{ccw}.mp4"), ccw);
        let vf = format!("scale=in_color_matrix=bt709:in_range=tv:out_range=full:{FLAGS}");
        let Some(e) = compare(&clip, &vf, 6, |_| {}) else { return };
        report(&format!("rotated (display_rotation {ccw} ccw)"), &e);
        let expect = rotated_size(640, 360, if ccw == 180 { 180 } else { 90 });
        assert_eq!(e.size, expect);
        assert!(e.mae.iter().all(|m| *m < 2.5), "{:?}", e.mae);
    }
}

#[test]
fn flip_mirrors() {
    if !have_ffmpeg() {
        return eprintln!("skipped: ffmpeg not found");
    }
    let clip = make_clip("c709_8.mp4", "yuv420p", TAG709, false);
    let vf = format!("scale=in_color_matrix=bt709:in_range=tv:out_range=full:{FLAGS},hflip");
    let Some(e) = compare(&clip, &vf, 4, |p| p.flip = true) else { return };
    report("flipped", &e);
    assert!(e.mae.iter().all(|m| *m < 2.5), "{:?}", e.mae);
}

#[test]
fn scaling_keeps_the_picture() {
    if !have_ffmpeg() {
        return eprintln!("skipped: ffmpeg not found");
    }
    let clip = make_clip("c709_8.mp4", "yuv420p", TAG709, false);
    let Some(mut dec) = open(&clip) else { return };
    let mut conv = Converter::new(&dec.gpu().device, &dec.gpu().context).unwrap();
    let f = dec.next_frame().unwrap().unwrap();
    let p = Params { colour: Colorimetry::from_vui(&dec.info().vui, 640, 360), rotation: 0, flip: false };
    let full = conv.create_target(640, 360).unwrap();
    let small = conv.create_target(320, 180).unwrap();
    conv.convert(&Source::from_frame(&f), &p, &full).unwrap();
    conv.convert(&Source::from_frame(&f), &p, &small).unwrap();
    let a = conv.read_rgba(&full).unwrap();
    let b = conv.read_rgba(&small).unwrap();
    // average each 2x2 block of the full size picture and compare with the half size one
    let mut sum = 0u64;
    for y in 0..180usize {
        for x in 0..320usize {
            for c in 0..3 {
                let at = |xx: usize, yy: usize| a[(yy * 640 + xx) * 4 + c] as u32;
                let avg = (at(2 * x, 2 * y) + at(2 * x + 1, 2 * y) + at(2 * x, 2 * y + 1) + at(2 * x + 1, 2 * y + 1)) / 4;
                sum += avg.abs_diff(b[(y * 320 + x) * 4 + c] as u32) as u64;
            }
        }
    }
    let mae = sum as f64 / (320.0 * 180.0 * 3.0);
    eprintln!("half size vs 2x2 average of full size: mean abs error {mae:.3}");
    assert!(mae < 2.0);
}

/// A 640x360 clip of one grey (10-bit luma code `y`, neutral chroma) tagged BT.2020 with `transfer`.
fn flat_clip(name: &str, y: u32, transfer: &str) -> std::path::PathBuf {
    let out = temp_dir().join(name);
    if !out.exists() {
        let tmp = out.with_extension("part.mp4");
        let st = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i", &format!("color=c=black:s=640x360:r=30:d=1,format=yuv420p10le,geq=lum={y}:cb=512:cr=512")])
            .args(["-c:v", "libx265", "-pix_fmt", "yuv420p10le", "-x265-params"])
            .arg(format!("log-level=error:colorprim=bt2020:transfer={transfer}:colormatrix=bt2020nc:range=limited"))
            .arg(&tmp)
            .status()
            .expect("ffmpeg");
        assert!(st.success());
        std::fs::rename(&tmp, &out).unwrap();
    }
    out
}

/// The green value of the centre pixel of the first picture of `clip`.
fn centre_grey(clip: &Path) -> Option<u8> {
    let mut dec = open(clip)?;
    let info = dec.info().clone();
    let mut conv = Converter::new(&dec.gpu().device, &dec.gpu().context).unwrap();
    let colour = Colorimetry::from_vui(&info.vui, info.width, info.height);
    let t = conv.create_target(info.width, info.height).unwrap();
    let f = dec.next_frame().unwrap().unwrap();
    conv.convert(&Source::from_frame(&f), &Params { colour, rotation: 0, flip: false }, &t).unwrap();
    let px = conv.read_rgba(&t).unwrap();
    let at = ((info.height / 2 * info.width + info.width / 2) * 4) as usize;
    assert!(px[at].abs_diff(px[at + 1]) <= 1 && px[at + 1].abs_diff(px[at + 2]) <= 1, "grey stays grey: {:?}", &px[at..at + 3]);
    Some(px[at + 1])
}

#[test]
fn pq_greys_are_tone_mapped() {
    if !have_ffmpeg() {
        return eprintln!("skipped: ffmpeg not found");
    }
    // PQ luma codes for 10, 100, 203, 400 and 1000+ nits; expected sRGB grey from the BT.2390
    // curve (source peak 1000 nits, target 203 nits = SDR white) worked out in a script
    let cases = [(327u32, 63i32), (509, 186), (573, 229), (636, 250), (723, 255)];
    for (y, want) in cases {
        let Some(got) = centre_grey(&flat_clip(&format!("flatb_pq_{y}.mp4"), y, "smpte2084")) else { return };
        eprintln!("PQ code {y}: grey {got} (expected about {want})");
        assert!((got as i32 - want).abs() <= 3, "PQ code {y}: {got} vs {want}");
    }
}

#[test]
fn hlg_grey_is_tone_mapped() {
    if !have_ffmpeg() {
        return eprintln!("skipped: ffmpeg not found");
    }
    // HLG signal 0.75 (about 75 % = reference white 203 nits on a 1000 nit display) must land in
    // the upper-middle of the SDR range, and 1.0 at white
    let mid = centre_grey(&flat_clip("flatb_hlg_724.mp4", 724, "arib-std-b67")).unwrap_or(0);
    let top = centre_grey(&flat_clip("flatb_hlg_940.mp4", 940, "arib-std-b67")).unwrap_or(0);
    eprintln!("HLG codes 724 (~0.75) and 940 (1.0): grey {mid} and {top}");
    assert!((180..=250).contains(&mid), "{mid}");
    assert!(top >= 250, "{top}");
}
