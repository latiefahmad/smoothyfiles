//! Audio decoding against ffmpeg: PCM in MOV bit for bit, AAC through the OS decoder within a
//! tolerance (the two decoders round differently), the timing of the first sample and the
//! length, and seeking. Run with `--nocapture` for the figures. Tests skip when ffmpeg or the
//! OS AAC decoder is missing.

#![cfg(windows)]

mod common;

use std::path::Path;

use common::av::*;
use common::have_ffmpeg;
use rustdecoder::audio::AudioDecoder;

struct Diff {
    max: f32,
    rms: f64,
    n: usize,
}

fn diff(a: &[f32], b: &[f32]) -> Diff {
    let n = a.len().min(b.len());
    let mut max = 0f32;
    let mut sum = 0f64;
    for i in 0..n {
        let d = (a[i] - b[i]).abs();
        max = max.max(d);
        sum += (d as f64) * (d as f64);
    }
    Diff { max, rms: (sum / n.max(1) as f64).sqrt(), n }
}

fn open(clip: &Path) -> Option<AudioDecoder> {
    match AudioDecoder::open(clip) {
        Ok(Some(d)) => Some(d),
        Ok(None) => panic!("no audio track in {clip:?}"),
        Err(rustdecoder::Error::Gpu(m)) => {
            eprintln!("skipped: {m}");
            None
        }
        Err(e) => panic!("open: {e}"),
    }
}

/// Decodes `clip` both ways and compares. Returns (diff, our frames, ffmpeg frames).
fn compare(name: &str, rate: u32, stereo: bool, acodec: &[&str]) -> Option<(Diff, usize, usize, u32)> {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return None;
    }
    let clip = make_av_clip(name, rate, stereo, acodec);
    let mut dec = open(&clip)?;
    let info = dec.info().clone();
    let (first, ours) = dec.decode_all().unwrap();
    let theirs = ffmpeg_audio(&clip);
    let ch = info.channels as usize;
    let start = ffprobe_audio_start(&clip);
    println!(
        "{name}: {} {} Hz {} ch; ours {} frames from {first:.6} s, ffmpeg {} frames (ffprobe start {start:.6} s)",
        info.codec,
        info.sample_rate,
        ch,
        ours.len() / ch,
        theirs.len() / ch
    );
    assert_eq!(info.sample_rate, rate);
    assert_eq!(ch, if stereo { 2 } else { 1 });
    // the first sample is the sample at time 0 (ffmpeg's start is 0 as well)
    assert!(first.abs() < 1.0 / rate as f64, "first sample at {first}");
    assert!(start.abs() < 0.001);
    let d = diff(&ours, &theirs);
    Some((d, ours.len() / ch, theirs.len() / ch, info.sample_rate))
}

#[test]
fn pcm_s16_is_bit_exact() {
    let Some((d, a, b, _)) = compare("av_pcm16.mov", 48000, true, PCM16) else { return };
    println!("pcm s16le: max diff {}, rms {}, {} samples", d.max, d.rms, d.n);
    assert_eq!(a, b);
    assert_eq!(d.max, 0.0);
}

#[test]
fn pcm_s24_is_bit_exact() {
    let Some((d, a, b, _)) = compare("av_pcm24.mov", 48000, true, PCM24) else { return };
    println!("pcm s24le: max diff {}, rms {}, {} samples", d.max, d.rms, d.n);
    assert_eq!(a, b);
    assert_eq!(d.max, 0.0);
}

#[test]
fn pcm_mono_44100_is_bit_exact() {
    let Some((d, a, b, _)) = compare("av_pcm16_mono.mov", 44100, false, PCM16) else { return };
    assert_eq!(a, b);
    assert_eq!(d.max, 0.0);
}

#[test]
fn aac_stereo_48k_matches_ffmpeg() {
    let Some((d, a, b, rate)) = compare("av_aac_st48.mp4", 48000, true, AAC) else { return };
    println!("aac stereo 48k: max abs diff {:.3e}, rms {:.3e} over {} samples; length {a} vs {b} frames", d.max, d.rms, d.n);
    assert!((a as i64 - b as i64).abs() <= 1024, "{a} vs {b}");
    let _ = rate;
    assert!(d.max < 2e-3, "max {}", d.max);
    assert!(d.rms < 2e-4, "rms {}", d.rms);
}

#[test]
fn aac_mono_44k_matches_ffmpeg() {
    let Some((d, a, b, _)) = compare("av_aac_mono44.mp4", 44100, false, AAC) else { return };
    println!("aac mono 44.1k: max abs diff {:.3e}, rms {:.3e} over {} samples; length {a} vs {b} frames", d.max, d.rms, d.n);
    assert!((a as i64 - b as i64).abs() <= 1024, "{a} vs {b}");
    assert!(d.max < 2e-3, "max {}", d.max);
    assert!(d.rms < 2e-4, "rms {}", d.rms);
}

/// Seeking decodes the same samples a linear decode gives from that time on.
fn seek_matches(name: &str, rate: u32, stereo: bool, acodec: &[&str], tol: f32) {
    if !have_ffmpeg() {
        return;
    }
    let clip = make_av_clip(name, rate, stereo, acodec);
    let Some(mut dec) = open(&clip) else { return };
    let ch = dec.info().channels as usize;
    let (_, all) = dec.decode_all().unwrap();
    for t in [0.0, 0.5, 1.0, 1.2345, 2.9] {
        dec.seek(t);
        let (first, got) = dec.decode_all().unwrap();
        let frame = (t * rate as f64).round() as usize;
        assert!((first - frame as f64 / rate as f64).abs() < 1e-9, "seek {t}: first sample at {first}");
        let want = &all[frame * ch..];
        assert_eq!(got.len(), want.len(), "seek {t}");
        // the last packets differ a little between a first and a later pass of the OS decoder
        // (drain at the end of the stream), so the final 2048 frames are left out
        let keep = got.len() - 2048 * ch;
        let d = diff(&got[..keep], &want[..keep]);
        println!("{name} seek {t}: first at {first:.6}, max diff to linear decode {:.3e}", d.max);
        assert!(d.max <= tol, "seek {t}: {}", d.max);
    }
}

#[test]
fn seek_pcm() {
    seek_matches("av_pcm16.mov", 48000, true, PCM16, 0.0);
}

#[test]
fn seek_aac() {
    seek_matches("av_aac_st48.mp4", 48000, true, AAC, 1e-4);
}

#[test]
fn tones_are_where_they_should_be() {
    // the 440 Hz left / 660 Hz right tones come out at those frequencies (zero crossings)
    if !have_ffmpeg() {
        return;
    }
    for (name, codec) in [("av_pcm16.mov", PCM16), ("av_aac_st48.mp4", AAC)] {
        let clip = make_av_clip(name, 48000, true, codec);
        let Some(mut dec) = open(&clip) else { return };
        let (_, pcm) = dec.decode_all().unwrap();
        for (ch, want) in [(0usize, 440.0f64), (1, 660.0)] {
            let x: Vec<f32> = pcm.iter().skip(ch).step_by(2).copied().collect();
            let (a, b) = (x.len() / 10, x.len() * 9 / 10);
            let n = (a..b - 1).filter(|&i| x[i] < 0.0 && x[i + 1] >= 0.0).count();
            let f = n as f64 / ((b - a) as f64 / 48000.0);
            println!("{name} channel {ch}: {f:.1} Hz");
            assert!((f - want).abs() < 3.0, "{name} ch {ch}: {f}");
        }
    }
}
