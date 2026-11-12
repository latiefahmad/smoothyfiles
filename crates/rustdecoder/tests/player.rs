//! The playback core on generated clips, with a fake clock: pictures come out in presentation
//! order and are not repeated beyond what the timing asks for, decoding ahead stays inside the
//! ring, and a seek lands on the picture whose time is the largest not after the target, with
//! exactly the pixels a linear decode gives. Tests skip with a message when ffmpeg or a GPU
//! decoder is missing.

#![cfg(windows)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use rustdecoder::VideoDecoder;
use rustdecoder::player::{FakeTime, Player};
use rustdecoder::present::{Colorimetry, Converter, Params};

const OPEN_GOP: &str = "bframes=6:b-pyramid=1:keyint=24:min-keyint=24:open-gop=1:ref=5";

fn clip() -> Option<std::path::PathBuf> {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return None;
    }
    Some(make_clip("m10_open.mp4", "yuv420p10le", OPEN_GOP, true))
}

/// The same checks on H.264: 8-bit, B-frames with a pyramid, an IDR every 24 pictures.
fn clip_h264() -> Option<std::path::PathBuf> {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return None;
    }
    Some(make_h264_clip("p_h264_gop24.mp4", "bframes=3:b-pyramid=normal:keyint=24:min-keyint=24:ref=4:scenecut=0", true))
}

fn open_player(p: &Path, ring: usize, time: Arc<FakeTime>) -> Option<Player> {
    match Player::open_with(p, ring, time) {
        Ok(p) => Some(p),
        Err(rustdecoder::Error::Gpu(m)) => {
            eprintln!("skipped: {m}");
            None
        }
        Err(e) => panic!("open: {e}"),
    }
}

/// Every picture of the clip in display order with a checksum of its RGBA pixels, by a plain
/// linear decode.
fn linear(p: &Path) -> Vec<(i64, u32)> {
    let mut dec = VideoDecoder::open(p).unwrap();
    let info = dec.info().clone();
    let mut conv = Converter::new(&dec.gpu().device, &dec.gpu().context).unwrap();
    let params = Params { colour: Colorimetry::from_vui(&info.vui, info.width, info.height), rotation: 0, flip: false };
    let t = conv.create_target(info.width, info.height).unwrap();
    let mut v = Vec::new();
    while let Some(f) = dec.next_frame().unwrap() {
        conv.convert(&rustdecoder::present::Source::from_frame(&f), &params, &t).unwrap();
        v.push((f.pts, crc32fast::hash(&conv.read_rgba(&t).unwrap())));
    }
    v
}

fn plays_in_order_without_extra_repeats(clip: std::path::PathBuf) {
    let time = Arc::new(FakeTime::new());
    let Some(mut pl) = open_player(&clip, 6, time.clone()) else { return };
    let all = linear(&clip);
    let ts = pl.info().timescale as f64;
    let (start, _) = pl.range();
    // a 30 fps clip polled at 60 Hz: every picture is on show for exactly two polls
    let mut shown: Vec<(u64, f64)> = Vec::new(); // (serial, pts) per poll
    pl.clock.start(start);
    let mut polls = 0;
    while !pl.finished() || polls < 3 {
        let now = pl.clock.time();
        assert!(pl.wait_ahead(now, Duration::from_secs(60)), "decoder stalled at {now}");
        if let Some(f) = pl.frame_now() {
            assert!(f.pts <= now + 1e-9, "a picture from the future: pts {} at {now}", f.pts);
            shown.push((f.serial, f.pts));
        }
        time.advance(1.0 / 60.0);
        polls += 1;
        if polls > 1000 {
            break;
        }
        // the last picture stays on show until the stream's end; stop one frame after it
        if pl.finished() && now > all.last().unwrap().0 as f64 / ts + 1.0 / 30.0 {
            break;
        }
    }
    let mut distinct: Vec<(u64, f64, usize)> = Vec::new();
    for (s, p) in &shown {
        match distinct.last_mut() {
            Some(l) if l.0 == *s => l.2 += 1,
            _ => distinct.push((*s, *p, 1)),
        }
    }
    let want: Vec<f64> = all.iter().map(|(p, _)| *p as f64 / ts).collect();
    let got: Vec<f64> = distinct.iter().map(|d| d.1).collect();
    assert_eq!(got.len(), want.len(), "pictures shown");
    for (g, w) in got.iter().zip(&want) {
        assert!((g - w).abs() < 1e-9, "order: {g} vs {w}");
    }
    assert!(want.windows(2).all(|w| w[0] < w[1]));
    // the last picture is on show for however long we kept polling; the others exactly twice
    for d in &distinct[..distinct.len() - 1] {
        assert_eq!(d.2, 2, "picture at {} shown for {} polls", d.1, d.2);
    }
    let st = pl.stats();
    eprintln!("{} pictures in order, each on show for 2 polls; stats {st:?}", distinct.len());
    assert_eq!((st.dropped, st.late), (0, 0));
    assert_eq!(st.shown as usize, want.len());
}

fn slow_polling_drops_and_never_goes_back(clip: std::path::PathBuf) {
    let time = Arc::new(FakeTime::new());
    let Some(mut pl) = open_player(&clip, 6, time.clone()) else { return };
    let (start, end) = pl.range();
    pl.clock.start(start);
    let mut last = f64::MIN;
    let mut n = 0;
    // 10 Hz polling of 30 fps: two of every three pictures are dropped
    while pl.clock.time() < end - 0.2 {
        let now = pl.clock.time();
        assert!(pl.wait_ahead(now, Duration::from_secs(60)));
        let f = pl.frame_now().expect("a picture");
        assert!(f.pts <= now + 1e-9 && f.pts > last, "pts {} after {last} at {now}", f.pts);
        assert!(now - f.pts < 1.0 / 30.0 + 1e-9, "picture {} behind at {now}", f.pts);
        last = f.pts;
        n += 1;
        time.advance(0.1);
    }
    let st = pl.stats();
    eprintln!("10 Hz polling: {n} shown, {} dropped, {} late", st.dropped, st.late);
    assert_eq!(st.shown as usize, n);
    assert!(st.dropped as usize >= n * 2 - 3 && st.dropped as usize <= n * 2 + 3, "{st:?}");
    assert_eq!(st.late, 0);
}

fn decode_ahead_stays_in_the_ring(clip: std::path::PathBuf) {
    for ring in [3usize, 4, 8] {
        let time = Arc::new(FakeTime::new());
        let Some(mut pl) = open_player(&clip, ring, time.clone()) else { return };
        // nothing consumes: the worker fills the ring and stops
        let t0 = std::time::Instant::now();
        while pl.in_flight() < ring && t0.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(300)); // and it stays there
        pl.wait_ahead(0.0, Duration::from_millis(1));
        assert_eq!(pl.error(), None);
        let st = pl.stats();
        eprintln!("ring {ring}: idle decode ahead {} slots, {} pictures decoded", pl.in_flight(), st.decoded);
        assert_eq!(pl.in_flight(), ring);
        assert_eq!(st.max_in_flight, ring);
        assert!(st.decoded as usize <= ring + 20, "decoder ran on without room: {}", st.decoded);
        // play it all at 4x polling pace: still never above the ring
        let (start, end) = pl.range();
        pl.clock.start(start);
        while pl.clock.time() < end {
            let now = pl.clock.time();
            pl.wait_ahead(now, Duration::from_secs(60));
            pl.frame_now();
            assert!(pl.in_flight() <= ring);
            time.advance(1.0 / 120.0);
        }
        assert!(pl.stats().max_in_flight <= ring);
    }
}

fn seek_lands_on_the_right_picture(clip: std::path::PathBuf) {
    let all = linear(&clip);
    let time = Arc::new(FakeTime::new());
    let Some(mut pl) = open_player(&clip, 5, time.clone()) else { return };
    let ts = pl.info().timescale as f64;
    let info = pl.info().clone();
    let mut conv = Converter::new(pl.device(), pl.context()).unwrap();
    let params = Params { colour: Colorimetry::from_vui(&info.vui, info.width, info.height), rotation: 0, flip: false };
    let target = conv.create_target(info.width, info.height).unwrap();
    let (start, end) = pl.range();
    let step = 1.0 / info.frame_rate;
    // exactly on pictures, between them, near GOP borders (every 24 pictures), before the start,
    // past the end, and backwards
    let mut targets = vec![start, start + 0.5 * step, 1.0, 1.0 + 0.4 * step, 23.0 * step, 24.0 * step, 24.0 * step - 1e-4, 25.0 * step, 47.5 * step, 48.0 * step, 49.0 * step, 50.3 * step, 71.0 * step, 72.0 * step, 73.0 * step, 2.9, end + 5.0, -3.0, 0.31];
    targets.extend((0..12).map(|i| (i * 7 % 90) as f64 * step + 0.003));
    let mut checked = 0;
    for t in targets {
        let t_clamped = t.clamp(start, end);
        let ticks = (t_clamped * ts + 1e-6).floor() as i64;
        let want = all.iter().filter(|(p, _)| *p <= ticks).max_by_key(|(p, _)| *p).copied().unwrap_or_else(|| *all.iter().min_by_key(|(p, _)| *p).unwrap());
        pl.seek(t);
        assert!((pl.clock.time() - t_clamped).abs() < 1e-9);
        assert!(pl.wait_ahead(t_clamped, Duration::from_secs(60)), "no picture after seek to {t}");
        let f = pl.frame_for(t_clamped).unwrap_or_else(|| panic!("no picture for seek to {t}"));
        let got = (f.pts * ts).round() as i64;
        assert_eq!(got, want.0, "seek to {t:.4} s: got pts {got}, expected {}", want.0);
        // and it is the same picture, pixel for pixel, as in the linear decode
        conv.convert(&f.source(), &params, &target).unwrap();
        let crc = crc32fast::hash(&conv.read_rgba(&target).unwrap());
        assert_eq!(crc, want.1, "seek to {t:.4} s: pixels of pts {got} differ from the linear decode");
        // the pictures after it follow in order
        let next = pl.frame_for(t_clamped + 1.5 * step);
        if let Some(n) = next {
            assert!(n.pts > f.pts || n.serial == f.serial);
        }
        checked += 1;
    }
    let st = pl.stats();
    eprintln!("{checked} seeks landed on the right picture with the right pixels; stats {st:?}");
    assert!(st.skipped_by_seek > 0);
    assert!(st.max_in_flight <= 5);
}

fn seek_then_play_on(clip: std::path::PathBuf) {
    let all = linear(&clip);
    let time = Arc::new(FakeTime::new());
    let Some(mut pl) = open_player(&clip, 5, time.clone()) else { return };
    let ts = pl.info().timescale as f64;
    pl.clock.start(0.0);
    pl.seek(1.5);
    let mut seen = Vec::new();
    for _ in 0..60 {
        let now = pl.clock.time();
        assert!(pl.wait_ahead(now, Duration::from_secs(60)));
        if let Some(f) = pl.frame_now()
            && seen.last().is_none_or(|l| *l != f.serial)
        {
            seen.push(f.serial);
            let want = all.iter().filter(|(p, _)| (*p as f64 / ts) <= now + 1e-9).max_by_key(|(p, _)| *p).unwrap();
            assert_eq!((f.pts * ts).round() as i64, want.0, "at {now}");
        }
        time.advance(1.0 / 60.0);
    }
    assert!(seen.len() >= 25);
}

macro_rules! both {
    ($($name:ident),*) => {
        mod hevc {
            use super::*;
            $(#[test] fn $name() { if let Some(c) = clip() { super::$name(c) } })*
        }
        mod h264 {
            use super::*;
            $(#[test] fn $name() { if let Some(c) = clip_h264() { super::$name(c) } })*
        }
    };
}

both!(plays_in_order_without_extra_repeats, slow_polling_drops_and_never_goes_back, decode_ahead_stays_in_the_ring, seek_lands_on_the_right_picture, seek_then_play_on);

/// ProRes 422 (decoded on the CPU, every picture a sync sample): the same checks.
fn clip_prores() -> Option<std::path::PathBuf> {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return None;
    }
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let out = temp_dir().join("p_prores_hq.mov");
    if !out.exists() {
        let tmp = out.with_file_name("part-p_prores_hq.mov");
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30", "-t", "3", "-vf", "noise=alls=6:allf=t"])
            .args(["-c:v", "prores_ks", "-profile:v", "3", "-pix_fmt", "yuv422p10le", "-f", "mov"])
            .arg(&tmp)
            .status()
            .expect("ffmpeg");
        assert!(status.success(), "ffmpeg could not make the ProRes clip");
        std::fs::rename(&tmp, &out).unwrap();
    }
    Some(out)
}

mod prores {
    use super::*;
    #[test]
    fn plays_in_order_without_extra_repeats() {
        if let Some(c) = clip_prores() {
            super::plays_in_order_without_extra_repeats(c)
        }
    }
    #[test]
    fn slow_polling_drops_and_never_goes_back() {
        if let Some(c) = clip_prores() {
            super::slow_polling_drops_and_never_goes_back(c)
        }
    }
    #[test]
    fn decode_ahead_stays_in_the_ring() {
        if let Some(c) = clip_prores() {
            super::decode_ahead_stays_in_the_ring(c)
        }
    }
    #[test]
    fn seek_lands_on_the_right_picture() {
        if let Some(c) = clip_prores() {
            super::seek_lands_on_the_right_picture(c)
        }
    }
    #[test]
    fn seek_then_play_on() {
        if let Some(c) = clip_prores() {
            super::seek_then_play_on(c)
        }
    }
}

/// DNxHR HQ (decoded on the CPU, every picture a sync sample): the same checks.
fn clip_dnxhr() -> Option<std::path::PathBuf> {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return None;
    }
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let out = temp_dir().join("p_dnxhr_hq.mov");
    if !out.exists() {
        let tmp = out.with_file_name("part-p_dnxhr_hq.mov");
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30", "-t", "3", "-vf", "noise=alls=6:allf=t"])
            .args(["-c:v", "dnxhd", "-profile:v", "dnxhr_hq", "-pix_fmt", "yuv422p", "-f", "mov"])
            .arg(&tmp)
            .status()
            .expect("ffmpeg");
        assert!(status.success(), "ffmpeg could not make the DNxHR clip");
        std::fs::rename(&tmp, &out).unwrap();
    }
    Some(out)
}

mod dnxhr {
    use super::*;
    #[test]
    fn plays_in_order_without_extra_repeats() {
        if let Some(c) = clip_dnxhr() {
            super::plays_in_order_without_extra_repeats(c)
        }
    }
    #[test]
    fn slow_polling_drops_and_never_goes_back() {
        if let Some(c) = clip_dnxhr() {
            super::slow_polling_drops_and_never_goes_back(c)
        }
    }
    #[test]
    fn decode_ahead_stays_in_the_ring() {
        if let Some(c) = clip_dnxhr() {
            super::decode_ahead_stays_in_the_ring(c)
        }
    }
    #[test]
    fn seek_lands_on_the_right_picture() {
        if let Some(c) = clip_dnxhr() {
            super::seek_lands_on_the_right_picture(c)
        }
    }
    #[test]
    fn seek_then_play_on() {
        if let Some(c) = clip_dnxhr() {
            super::seek_then_play_on(c)
        }
    }
}
