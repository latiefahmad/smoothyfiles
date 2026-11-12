//! Video and audio together. The sync tests drive a [`Playback`] with a fake audio sink whose
//! clock is a fake time, so every step is exact: pictures come out in presentation order next
//! to the audio clock, the audio the feed thread queues is contiguous, and after a seek both
//! sides land at the target. One more test opens the real WASAPI output muted (nothing is
//! audible: session volume 0, session mute, silent buffers) and checks its clock.
//! Tests skip with a message when ffmpeg, a GPU decoder or an audio device is missing.

#![cfg(windows)]

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::av::*;
use common::have_ffmpeg;
use rustdecoder::audio_out::{AudioOutput, AudioSink, OutputOptions};
use rustdecoder::player::{FakeTime, TimeSource};
use rustdecoder::playback::Playback;

#[derive(Debug, Clone, PartialEq)]
enum Ev {
    Flush,
    Push { pts: f64, frames: usize },
}

#[derive(Default)]
struct St {
    rate: u32,
    channels: usize,
    log: Vec<Ev>,
    /// Seconds of sound pushed since the last flush.
    pushed: f64,
    base: Option<f64>,
    running: bool,
    played: f64,
    since: f64,
    ended: bool,
    /// The most recent block pushed, and the channel count it came with.
    last: Vec<f32>,
}

/// A sink that plays nothing and counts time by a fake clock.
struct FakeSink {
    time: Arc<FakeTime>,
    st: Mutex<St>,
}

impl FakeSink {
    fn new(time: Arc<FakeTime>) -> Arc<Self> {
        Arc::new(FakeSink { time, st: Mutex::new(St::default()) })
    }
    fn played(&self, s: &St) -> f64 {
        if s.running { s.played + (self.time.now() - s.since) } else { s.played }
    }
    fn last(&self) -> (usize, Vec<f32>) {
        let s = self.st.lock().unwrap();
        (s.channels, s.last.clone())
    }
    fn log(&self) -> Vec<Ev> {
        self.st.lock().unwrap().log.clone()
    }
}

impl AudioSink for FakeSink {
    fn set_input(&self, rate: u32, channels: usize) {
        let mut s = self.st.lock().unwrap();
        s.rate = rate;
        s.channels = channels;
    }
    fn push(&self, pts: f64, pcm: &[f32]) {
        let mut s = self.st.lock().unwrap();
        let frames = pcm.len() / s.channels;
        s.base.get_or_insert(pts);
        s.pushed += frames as f64 / s.rate as f64;
        s.last = pcm.to_vec();
        s.log.push(Ev::Push { pts, frames });
    }
    fn end_of_data(&self) {
        self.st.lock().unwrap().ended = true;
    }
    fn buffered(&self) -> f64 {
        let s = self.st.lock().unwrap();
        (s.pushed - self.played(&s)).max(0.0)
    }
    fn flush(&self) {
        let mut s = self.st.lock().unwrap();
        s.log.push(Ev::Flush);
        s.pushed = 0.0;
        s.played = 0.0;
        s.running = false;
        s.base = None;
        s.ended = false;
    }
    fn play(&self) {
        let mut s = self.st.lock().unwrap();
        if !s.running {
            s.since = self.time.now();
            s.running = true;
        }
    }
    fn pause(&self) {
        let mut s = self.st.lock().unwrap();
        if s.running {
            s.played += self.time.now() - s.since;
            s.running = false;
        }
    }
    fn position(&self) -> Option<f64> {
        let s = self.st.lock().unwrap();
        s.base.map(|b| b + self.played(&s))
    }
    fn time_source(&self) -> Arc<dyn TimeSource> {
        self.time.clone()
    }
}

fn clip() -> Option<PathBuf> {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return None;
    }
    Some(make_av_clip("av_aac_st48.mp4", 48000, true, AAC))
}

fn clip_h264() -> Option<PathBuf> {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return None;
    }
    Some(make_av_clip_h264("av_h264_aac_st48.mp4", 48000, true, AAC))
}

fn open(p: &std::path::Path, sink: Arc<FakeSink>) -> Option<Playback> {
    match Playback::open_with_sink(p, 6, Some(sink)) {
        Ok(p) => Some(p),
        Err(rustdecoder::Error::Gpu(m)) => {
            eprintln!("skipped: {m}");
            None
        }
        Err(e) => panic!("open: {e}"),
    }
}

/// One step of a 60 Hz display: time moves, the decoder is given time to be ahead, the picture
/// for the clock is asked for.
fn step(pb: &mut Playback, time: &FakeTime) -> Option<rustdecoder::player::Shown> {
    // the audio feed is a thread too: with a fake time that races ahead of real time it has to
    // be given the time to queue what the clock is about to play
    for _ in 0..1000 {
        if pb.audio_ended() || pb.sink().is_none_or(|s| s.buffered() > 0.05) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    time.advance(1.0 / 60.0);
    let now = pb.time();
    pb.player.wait_ahead(now, Duration::from_secs(5));
    pb.frame()
}

fn pictures_follow_the_audio_clock_in_order(clip: PathBuf) {
    let time = Arc::new(FakeTime::new());
    let sink = FakeSink::new(time.clone());
    let Some(mut pb) = open(&clip, sink.clone()) else { return };
    assert!(pb.has_audio(), "{}", pb.audio_note());
    pb.play();
    let mut last_serial = 0;
    let mut last_pts = -1.0;
    let mut shown = 0;
    let mut worst_lag = 0.0f64;
    for _ in 0..150 {
        let Some(f) = step(&mut pb, &time) else { continue };
        let now = pb.time();
        assert!(f.pts <= now + 1e-9, "a picture from the future: {} at {now}", f.pts);
        worst_lag = worst_lag.max(now - f.pts);
        if f.serial != last_serial {
            assert!(f.pts > last_pts, "pictures out of order: {} after {last_pts}", f.pts);
            last_pts = f.pts;
            last_serial = f.serial;
            shown += 1;
        }
        // the audio clock is the movie clock
        let a = pb.audio_position().unwrap();
        assert!((a - now).abs() < 1e-6, "audio {a} clock {now}");
    }
    let st = pb.stats();
    println!("2.5 s at 60 Hz: {shown} pictures, dropped {}, late {}, worst lag {:.1} ms", st.dropped, st.late, worst_lag * 1000.0);
    assert!(shown >= 70 && shown <= 76, "{shown} pictures in 2.5 s of 30 fps");
    assert_eq!(st.dropped, 0);
    // (Matroska times are whole milliseconds: a frame can sit up to 0.7 ms off its true time)
    assert!(worst_lag <= 1.0 / 30.0 + 0.002);
    // the sound queued is one unbroken run from time 0
    let log = sink.log();
    let mut next = 0.0;
    let mut total = 0usize;
    for e in &log {
        match e {
            Ev::Push { pts, frames } => {
                assert!((pts - next).abs() < 1e-6, "audio gap: block at {pts}, expected {next}");
                next = pts + *frames as f64 / 48000.0;
                total += frames;
            }
            Ev::Flush => panic!("flush without a seek"),
        }
    }
    println!("audio queued: {total} frames ({next:.3} s) in {} blocks", log.len());
    assert!(next >= 2.5, "audio ahead of the clock");
}

fn seeking_lands_both_sides_on_the_target(clip: PathBuf) {
    let time = Arc::new(FakeTime::new());
    let sink = FakeSink::new(time.clone());
    let Some(mut pb) = open(&clip, sink.clone()) else { return };
    pb.play();
    for _ in 0..20 {
        step(&mut pb, &time);
    }
    for (n, target) in [1.5f64, 0.2, 2.9, 0.0].into_iter().enumerate() {
        pb.seek(target);
        assert!(pb.is_playing());
        assert!((pb.time() - target).abs() < 1e-9, "clock at {} after seek to {target}", pb.time());
        // audio: flushed, then the first block queued starts exactly at the target
        let mut first_block = None;
        for _ in 0..200 {
            let log = sink.log();
            // seek() returns after the feed thread flushed: the first block after the last
            // flush is where the audio restarted
            let flush = log.iter().rposition(|e| *e == Ev::Flush).expect("no flush");
            if let Some(e) = log[flush..].iter().find(|e| matches!(e, Ev::Push { .. })) {
                first_block = Some(e.clone());
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let Some(Ev::Push { pts, .. }) = first_block else { panic!("no audio after the seek to {target}") };
        assert!((pts - target).abs() < 1.0 / 48000.0, "audio restarts at {pts}, target {target}");
        // video: the first picture is the one at or just before the target
        let mut first = None;
        for _ in 0..30 {
            if let Some(f) = step(&mut pb, &time) {
                first = Some(f);
                break;
            }
        }
        let f = first.expect("no picture after the seek");
        let after = pb.time();
        println!("seek {n} to {target}: audio restarts at {pts:.6}, first picture {:.4} (clock {after:.4})", f.pts);
        assert!(f.pts <= after + 1e-9 && after - f.pts <= 1.0 / 30.0 + 1e-6, "picture {} for clock {after}", f.pts);
        // and they keep going together
        let mut last = f.pts;
        for _ in 0..30 {
            if let Some(g) = step(&mut pb, &time)
                && g.pts != last
            {
                assert!(g.pts > last, "out of order after a seek");
                last = g.pts;
            }
        }
        let a = pb.audio_position().unwrap();
        assert!((a - pb.time()).abs() < 1e-6, "audio {a} clock {}", pb.time());
    }
}

#[test]
fn paused_playback_stands_still() {
    let Some(clip) = clip() else { return };
    let time = Arc::new(FakeTime::new());
    let sink = FakeSink::new(time.clone());
    let Some(mut pb) = open(&clip, sink) else { return };
    pb.play();
    for _ in 0..30 {
        step(&mut pb, &time);
    }
    pb.pause();
    let t = pb.time();
    let a = pb.audio_position().unwrap();
    time.advance(5.0);
    assert_eq!(pb.time(), t);
    assert_eq!(pb.audio_position().unwrap(), a);
    pb.play();
    time.advance(0.5);
    assert!((pb.time() - t - 0.5).abs() < 1e-6);
}

#[test]
fn no_audio_track_keeps_the_system_clock() {
    if !have_ffmpeg() {
        return;
    }
    let clip = common::make_clip("m10_plain.mp4", "yuv420p10le", "bframes=2", false);
    let time = Arc::new(FakeTime::new());
    let sink = FakeSink::new(time);
    let Some(pb) = open(&clip, sink) else { return };
    assert!(!pb.has_audio());
    assert_eq!(pb.audio_note(), "no audio track");
}

#[test]
fn muted_device_clock_runs_in_real_time() {
    let out = match AudioOutput::open(OutputOptions { muted: true, volume: 1.0 }) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    let (rate, ch) = out.device_format();
    println!("device: {} Hz {} ch ({})", rate, ch, out.device_name());
    let src_rate = 44100u32; // different from the device's on most machines: the resampler runs
    out.set_input(src_rate, 2);
    let tone: Vec<f32> = (0..src_rate as usize * 2).flat_map(|i| {
        let v = (i as f64 * 2.0 * std::f64::consts::PI * 440.0 / src_rate as f64).sin() as f32 * 0.5;
        [v, v]
    }).collect();
    out.push(10.0, &tone);
    assert!((out.buffered() - 2.0).abs() < 0.05, "buffered {}", out.buffered());
    let clock = out.time_source();
    let c0 = clock.now();
    out.play();
    let t0 = std::time::Instant::now();
    let mut last = c0;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(25));
        let c = clock.now();
        assert!(c >= last, "clock went back: {c} after {last}");
        last = c;
    }
    let wall = t0.elapsed().as_secs_f64();
    let adv = clock.now() - c0;
    let pos = out.position().unwrap();
    println!("muted device: clock advanced {adv:.4} s in {wall:.4} s wall, position {pos:.4}, {} device periods, {} under-runs", out.device_periods(), out.underruns());
    assert!((adv - wall).abs() < 0.06, "clock {adv} wall {wall}");
    assert!((pos - 10.0 - adv).abs() < 0.03);
    assert!(out.device_periods() > 5);
    // pause: the clock stops
    out.pause();
    let p = clock.now();
    std::thread::sleep(Duration::from_millis(200));
    assert!((clock.now() - p).abs() < 1e-6);
    // flush and a new position
    out.flush();
    assert!(out.position().is_none());
    out.push(20.0, &tone[..44100]);
    out.play();
    std::thread::sleep(Duration::from_millis(300));
    let pos = out.position().unwrap();
    println!("after flush: position {pos:.4} (started at 20.0)");
    assert!(pos > 20.2 && pos < 20.45, "{pos}");
    // the queue ran out (0.5 s pushed); the stalled clock does not run on
    out.pause();
}

#[test]
fn muted_device_under_run_stops_the_clock() {
    let out = match AudioOutput::open(OutputOptions::default()) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    let (rate, ch) = out.device_format();
    out.set_input(rate, ch);
    // 0.3 s of sound, then nothing: the device plays silence, the clock stands at 0.3 s
    out.push(0.0, &vec![0.0f32; (rate as usize * 3 / 10) * ch]);
    let clock = out.time_source();
    let c0 = clock.now();
    out.play();
    std::thread::sleep(Duration::from_millis(900));
    let adv = clock.now() - c0;
    println!("0.3 s queued, 0.9 s wall: clock advanced {adv:.3} s, under-runs {}", out.underruns());
    assert!(adv > 0.25 && adv < 0.36, "{adv}");
    assert!(out.underruns() >= 1);
    // end of data: silence is time again
    out.end_of_data();
    let c1 = clock.now();
    std::thread::sleep(Duration::from_millis(600));
    assert!(clock.now() - c1 > 0.3, "{}", clock.now() - c1);
    out.pause();
}

/// The sync tests on HEVC and on H.264.
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

both!(pictures_follow_the_audio_clock_in_order, seeking_lands_both_sides_on_the_target);

// ------------------------------------------------------------------ Matroska / WebM

const MKV_H264: &[&str] = &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-preset", "veryfast", "-x264-params", "log-level=error:keyint=15:min-keyint=15:bframes=3:scenecut=0"];
const MKV_HEVC: &[&str] = &["-c:v", "libx265", "-pix_fmt", "yuv420p", "-preset", "ultrafast", "-x265-params", "log-level=error:keyint=15:min-keyint=15"];
const MKV_VP9: &[&str] = &["-c:v", "libvpx-vp9", "-pix_fmt", "yuv420p", "-deadline", "good", "-cpu-used", "5", "-g", "15", "-b:v", "1M"];

fn mkv_clip(name: &str, video: &[&str], audio: &[&str]) -> Option<PathBuf> {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return None;
    }
    Some(common::make_mkv(name, video, audio, &[]))
}

macro_rules! containers {
    ($($m:ident: $name:expr, $video:expr, $audio:expr;)*) => {
        $(mod $m {
            use super::*;
            fn clip() -> Option<PathBuf> { mkv_clip($name, $video, $audio) }
            #[test] fn pictures_follow_the_audio_clock_in_order() { if let Some(c) = clip() { super::pictures_follow_the_audio_clock_in_order(c) } }
            #[test] fn seeking_lands_both_sides_on_the_target() { if let Some(c) = clip() { super::seeking_lands_both_sides_on_the_target(c) } }
        })*
    };
}

containers! {
    mkv_h264_aac: "pb_h264_aac.mkv", MKV_H264, &["-c:a", "aac", "-b:a", "96k"];
    mkv_hevc_pcm: "pb_hevc_pcm.mkv", MKV_HEVC, &["-c:a", "pcm_s16le"];
    mkv_vp9_pcm: "pb_vp9_pcm.mkv", MKV_VP9, &["-c:a", "pcm_s16le"];
    mkv_vp9_aac: "pb_vp9_aac.mkv", MKV_VP9, &["-c:a", "aac", "-b:a", "96k"];
    webm_vp9_opus: "pb_vp9_opus.webm", MKV_VP9, &["-c:a", "libopus", "-b:a", "64k"];
    webm_vp9_vorbis: "pb_vp9_vorbis.webm", MKV_VP9, &["-c:a", "libvorbis", "-b:a", "64k"];
    mkv_h264_flac: "pb_h264_flac.mkv", MKV_H264, &["-c:a", "flac"];
    mkv_h264_mp3: "pb_h264_mp3.mkv", MKV_H264, &["-c:a", "libmp3lame", "-b:a", "128k"];
}

// ------------------------------------------------------------------ fragmented MP4

macro_rules! fragmented {
    ($($m:ident: $name:expr, $video:expr, $mux:expr;)*) => {
        $(mod $m {
            use super::*;
            fn clip() -> Option<PathBuf> {
                if !have_ffmpeg() {
                    eprintln!("skipped: ffmpeg not found");
                    return None;
                }
                Some(common::make_frag($name, $video, $mux))
            }
            #[test] fn pictures_follow_the_audio_clock_in_order() { if let Some(c) = clip() { super::pictures_follow_the_audio_clock_in_order(c) } }
            #[test] fn seeking_lands_both_sides_on_the_target() { if let Some(c) = clip() { super::seeking_lands_both_sides_on_the_target(c) } }
        })*
    };
}

// Only clips with negative composition offsets: with B-frames and plain offsets (no edit list) the
// first picture is delayed by the reorder depth while the sound starts at 0, and `Playback` starts
// its clock at the first picture, so the sound would play that much late (left for later).

fragmented! {
    frag_h264_empty_moov: "frag_h264_dash_neg.mp4", common::FRAG_H264, &["-movflags", "+dash+negative_cts_offsets"];
    frag_h264_mixed: "frag_h264_mixed_neg.mp4", common::FRAG_H264, &["-movflags", "frag_keyframe+negative_cts_offsets"];
    frag_hevc_cmaf: "frag_hevc_cmaf.mp4", common::FRAG_HEVC, &["-movflags", "+cmaf"];
    frag_h264_cmaf: "frag_h264_cmaf.mp4", common::FRAG_H264, &["-movflags", "+cmaf"];
}

/// A codec without a decoder on this machine: the video plays on the system clock and the
/// note says which codec and why (AC-3: Windows has no Dolby Digital decoder).
#[test]
fn mkv_without_a_decoder_plays_without_sound_and_says_so() {
    let name = "pb_vp9_ac3.mkv";
    let Some(clip) = mkv_clip(name, MKV_VP9, &["-c:a", "ac3", "-b:a", "128k"]) else { return };
    let time = Arc::new(FakeTime::new());
    let sink = FakeSink::new(time);
    let Some(mut pb) = open(&clip, sink) else { return };
    if pb.has_audio() {
        println!("{name}: this machine has an AC-3 decoder");
        return;
    }
    println!("{name}: audio note: {}", pb.audio_note());
    assert!(pb.audio_note().contains("not playable") && pb.audio_note().contains("AC-3"), "{}", pb.audio_note());
    pb.play();
    let t0 = std::time::Instant::now();
    let mut shown = 0;
    let mut last = -1.0;
    while t0.elapsed() < Duration::from_millis(1200) {
        pb.player.wait_ahead(pb.time(), Duration::from_millis(50));
        if let Some(f) = pb.frame()
            && f.pts != last
        {
            last = f.pts;
            shown += 1;
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    println!("{name}: {shown} pictures in 1.2 s of wall time, clock at {:.3}", pb.time());
    assert!(shown >= 25, "{shown}");
    assert!(pb.time() > 1.0 && pb.time() < 1.5);
}

// ------------------------------------------------------------------ ProRes in a MOV with PCM

/// A 3 s MOV: ProRes 422 HQ (decoded on the CPU) and 16-bit PCM, 48 kHz stereo, 30 fps.
fn clip_prores() -> Option<PathBuf> {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return None;
    }
    static LOCK: Mutex<()> = Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let out = common::temp_dir().join("av_prores_pcm_st48.mov");
    if out.exists() {
        return Some(out);
    }
    let tmp = out.with_file_name("part-av_prores_pcm_st48.mov");
    let status = std::process::Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30", "-f", "lavfi", "-i", "sine=f=440:r=48000:d=3", "-f", "lavfi", "-i", "sine=f=660:r=48000:d=3"])
        .args(["-filter_complex", "[1:a][2:a]amerge=inputs=2[a]", "-map", "0:v", "-map", "[a]", "-t", "3"])
        .args(["-c:v", "prores_ks", "-profile:v", "3", "-pix_fmt", "yuv422p10le", "-c:a", "pcm_s16le", "-f", "mov"])
        .arg(&tmp)
        .status()
        .expect("ffmpeg");
    assert!(status.success(), "ffmpeg could not make the ProRes clip");
    std::fs::rename(&tmp, &out).unwrap();
    Some(out)
}

/// MPEG transport stream: H.264 + AAC (ADTS) through the same sync and seek checks.
mod mpegts {
    use super::*;

    fn clip_ts() -> Option<PathBuf> {
        if !have_ffmpeg() {
            eprintln!("skipped: ffmpeg not found");
            return None;
        }
        let v = &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-preset", "veryfast", "-x264-params", "log-level=error:keyint=15:min-keyint=15:bframes=3:scenecut=0"];
        Some(common::make_av("pb_h264_aac.ts", v, AAC, &["-f", "mpegts"]))
    }
    #[test]
    fn pictures_follow_the_audio_clock_in_order() {
        if let Some(c) = clip_ts() {
            super::pictures_follow_the_audio_clock_in_order(c)
        }
    }
    #[test]
    fn seeking_lands_both_sides_on_the_target() {
        if let Some(c) = clip_ts() {
            super::seeking_lands_both_sides_on_the_target(c)
        }
    }
}

mod prores {
    use super::*;
    #[test]
    fn pictures_follow_the_audio_clock_in_order() {
        if let Some(c) = clip_prores() {
            super::pictures_follow_the_audio_clock_in_order(c)
        }
    }
    #[test]
    fn seeking_lands_both_sides_on_the_target() {
        if let Some(c) = clip_prores() {
            super::seeking_lands_both_sides_on_the_target(c)
        }
    }
}

// ------------------------------------------------------------------ DNxHR in a MOV with PCM

/// A 3 s MOV: DNxHR HQ (decoded on the CPU) and 16-bit PCM, 48 kHz stereo, 30 fps.
fn clip_dnxhr() -> Option<PathBuf> {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return None;
    }
    static LOCK: Mutex<()> = Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let out = common::temp_dir().join("av_dnxhr_pcm_st48.mov");
    if out.exists() {
        return Some(out);
    }
    let tmp = out.with_file_name("part-av_dnxhr_pcm_st48.mov");
    let status = std::process::Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30", "-f", "lavfi", "-i", "sine=f=440:r=48000:d=3", "-f", "lavfi", "-i", "sine=f=660:r=48000:d=3"])
        .args(["-filter_complex", "[1:a][2:a]amerge=inputs=2[a]", "-map", "0:v", "-map", "[a]", "-t", "3"])
        .args(["-c:v", "dnxhd", "-profile:v", "dnxhr_hq", "-pix_fmt", "yuv422p", "-c:a", "pcm_s16le", "-f", "mov"])
        .arg(&tmp)
        .status()
        .expect("ffmpeg");
    assert!(status.success(), "ffmpeg could not make the DNxHR clip");
    std::fs::rename(&tmp, &out).unwrap();
    Some(out)
}

mod dnxhr {
    use super::*;
    #[test]
    fn pictures_follow_the_audio_clock_in_order() {
        if let Some(c) = clip_dnxhr() {
            super::pictures_follow_the_audio_clock_in_order(c)
        }
    }
    #[test]
    fn seeking_lands_both_sides_on_the_target() {
        if let Some(c) = clip_dnxhr() {
            super::seeking_lands_both_sides_on_the_target(c)
        }
    }
}

/// What a default device change (or `AUDCLNT_E_DEVICE_INVALIDATED`) makes the output do, forced
/// through the test hook while a muted stream plays: the stream moves to a freshly opened one, the
/// clock goes on from where it was, the queued sound keeps flowing and the movie position follows.
#[test]
fn device_reopen_keeps_the_clock_and_the_sound() {
    let out = match AudioOutput::open(OutputOptions { muted: true, volume: 1.0 }) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    let src_rate = 44100u32;
    out.set_input(src_rate, 2);
    let tone: Vec<f32> = (0..src_rate as usize * 4).flat_map(|i| {
        let v = (i as f64 * 2.0 * std::f64::consts::PI * 440.0 / src_rate as f64).sin() as f32 * 0.5;
        [v, v]
    }).collect();
    out.push(10.0, &tone);
    let clock = out.time_source();
    out.play();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(out.reopens(), 0);
    for round in 1..=3u64 {
        let before = clock.now();
        let pos_before = out.position().unwrap();
        let periods = out.device_periods();
        let t0 = std::time::Instant::now();
        assert!(out.force_reopen(), "the reopen did not happen");
        let took = t0.elapsed().as_secs_f64();
        let after = clock.now();
        let pos_after = out.position().unwrap();
        // the clock never goes back and does not jump: only the time the reopen took passes (the
        // sound the old device had been given and not played went back into the queue)
        let step = after - before;
        println!("reopen {round}: took {took:.3} s, clock {before:.3} -> {after:.3} (+{step:.3}), position {pos_before:.3} -> {pos_after:.3}, buffered {:.3}", out.buffered());
        assert!(step >= 0.0 && step < took + 0.03, "clock stepped by {step}");
        assert!((pos_after - 10.0 - after).abs() < 0.03, "position {pos_after} against clock {after}");
        assert_eq!(out.reopens(), round);
        // runs on in real time, and the new stream is alive
        let t1 = std::time::Instant::now();
        std::thread::sleep(Duration::from_millis(400));
        let adv = clock.now() - after;
        let wall = t1.elapsed().as_secs_f64();
        assert!((adv - wall).abs() < 0.06, "after the reopen: clock {adv} wall {wall}");
        assert!(out.device_periods() > periods + 10, "no device periods on the new stream");
    }
    // the sound went on being consumed: 4 s pushed, the clock's seconds gone, the device buffer
    // (100 ms) filled
    let b = out.buffered();
    let expect = 4.0 - clock.now() - 0.1;
    assert!((b - expect).abs() < 0.08, "buffered {b}, expected {expect}");
    // a pause across a reopen stays a pause
    out.pause();
    let p = clock.now();
    assert!(out.force_reopen());
    let p2 = clock.now();
    assert!(p2 >= p && p2 - p < 0.1, "{p} {p2}");
    std::thread::sleep(Duration::from_millis(200));
    assert!((clock.now() - p2).abs() < 1e-6, "the clock ran while paused");
    // play, flush and a new position still work on the new stream
    out.flush();
    assert!(out.position().is_none());
    out.push(20.0, &tone[..44100 * 2]);
    out.play();
    std::thread::sleep(Duration::from_millis(300));
    let pos = out.position().unwrap();
    assert!(pos > 20.2 && pos < 20.45, "{pos}");
    out.pause();
}

// ------------------------------------------------------------------ several audio tracks

use common::tracks::*;

/// Runs the clock on until the sink has queued sound again after the last flush; the newest block.
fn wait_block(pb: &mut Playback, time: &FakeTime, sink: &FakeSink) -> (usize, Vec<f32>) {
    for _ in 0..100 {
        step(pb, time);
        if let Some(Ev::Push { .. }) = sink.log().last() {
            let (ch, pcm) = sink.last();
            if pcm.len() >= 1024 * ch {
                return (ch, pcm);
            }
        }
    }
    panic!("no sound queued");
}

#[test]
fn switching_the_audio_track_while_playing() {
    if !have_ffmpeg() {
        return eprintln!("skipped: ffmpeg not found");
    }
    let clip = two_languages("mkv", &["-c:a", "pcm_s16le"]);
    let time = Arc::new(FakeTime::new());
    let sink = FakeSink::new(time.clone());
    let Some(mut pb) = open(&clip, sink.clone()) else { return };
    assert_eq!(pb.audio_tracks().len(), 2);
    assert_eq!(pb.audio_track(), Some(1), "the default (English, 880 Hz) plays first");
    pb.play();
    for _ in 0..20 {
        step(&mut pb, &time);
    }
    let (ch, pcm) = wait_block(&mut pb, &time, &sink);
    assert!((zero_cross_freq(&pcm, ch, 48000) - 880.0).abs() < 8.0);
    let t = pb.time();
    let flushes = sink.log().iter().filter(|e| **e == Ev::Flush).count();
    let t0 = std::time::Instant::now();
    pb.set_audio_track(0).unwrap();
    let took = t0.elapsed();
    println!("switch took {took:?} at {t:.3} s");
    assert!(took < Duration::from_millis(250), "switch took {took:?}");
    assert_eq!(pb.audio_track(), Some(0));
    assert!(pb.is_playing());
    let log = sink.log();
    assert_eq!(log.iter().filter(|e| **e == Ev::Flush).count(), flushes + 1, "one flush for the switch");
    let flush = log.iter().rposition(|e| *e == Ev::Flush).unwrap();
    // the new track is queued from the clock time of the switch, no later than it, no sooner
    // than one sample before
    let Some(Ev::Push { pts, .. }) = log[flush..].iter().find(|e| matches!(e, Ev::Push { .. })).cloned() else { panic!("no sound after the switch") };
    assert!((pts - t).abs() < 1.0 / 48000.0 + 1e-9, "new track starts at {pts}, switch at {t}");
    let (ch, pcm) = wait_block(&mut pb, &time, &sink);
    assert!((zero_cross_freq(&pcm, ch, 48000) - 440.0).abs() < 8.0);
    // audio and video clock stay one clock
    let a = pb.audio_position().unwrap();
    assert!((a - pb.time()).abs() < 0.05, "audio {a} clock {}", pb.time());
    // and back, while paused
    pb.pause();
    pb.set_audio_track(1).unwrap();
    assert!(!pb.is_playing());
    pb.play();
    let (ch, pcm) = wait_block(&mut pb, &time, &sink);
    assert!((zero_cross_freq(&pcm, ch, 48000) - 880.0).abs() < 8.0);
    assert!(pb.set_audio_track(5).is_err());
}

#[test]
fn camera_tracks_play_as_front_pair_and_stay_aligned_over_a_seek() {
    if !have_ffmpeg() {
        return eprintln!("skipped: ffmpeg not found");
    }
    let clip = make("tracks_4mono.mxf", &[440, 550, 660, 770], common::tracks::PCM24, &[]);
    let time = Arc::new(FakeTime::new());
    let sink = FakeSink::new(time.clone());
    let Some(mut pb) = open(&clip, sink.clone()) else { return };
    assert_eq!(pb.audio_tracks().len(), 1);
    assert_eq!(pb.audio_tracks()[0].channels, 4);
    pb.play();
    let (ch, pcm) = wait_block(&mut pb, &time, &sink);
    // left and right are tracks 1 and 2
    assert_eq!(ch, 2);
    assert!((zero_cross_freq(&pcm, 2, 48000) - 440.0).abs() < 8.0);
    let right: Vec<f32> = pcm.chunks_exact(2).map(|f| f[1]).collect();
    assert!((zero_cross_freq(&right, 1, 48000) - 550.0).abs() < 8.0);
    for target in [1.0f64, 0.3] {
        pb.seek(target);
        let (_, pcm) = wait_block(&mut pb, &time, &sink);
        let right: Vec<f32> = pcm.chunks_exact(2).map(|f| f[1]).collect();
        assert!((zero_cross_freq(&pcm, 2, 48000) - 440.0).abs() < 8.0);
        assert!((zero_cross_freq(&right, 1, 48000) - 550.0).abs() < 8.0);
    }
}
