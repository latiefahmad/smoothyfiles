//! MPEG-2 video in a program stream with MP2 audio through [`Playback`]: the pictures come out in
//! presentation order next to the audio clock (a fake sink, a fake time, so every step is exact),
//! the MP2 sound (decoded by Windows' MPEG audio decoder) is one unbroken run, and a seek lands
//! both sides on the target. Nothing is audible. Skips with a message when ffmpeg, a GPU decoder
//! or the audio decoder is missing.

#![cfg(windows)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{have_ffmpeg, make_av};
use rustdecoder::audio_out::AudioSink;
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
    pushed: f64,
    base: Option<f64>,
    running: bool,
    played: f64,
    since: f64,
}

struct FakeSink {
    time: Arc<FakeTime>,
    st: Mutex<St>,
}

impl FakeSink {
    fn played(&self, s: &St) -> f64 {
        if s.running { s.played + (self.time.now() - s.since) } else { s.played }
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
        s.log.push(Ev::Push { pts, frames });
    }
    fn end_of_data(&self) {}
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

fn step(pb: &mut Playback, time: &FakeTime) -> Option<rustdecoder::player::Shown> {
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

#[test]
fn mpeg2_ps_with_mp2_plays_and_seeks() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let v = ["-c:v", "mpeg2video", "-pix_fmt", "yuv420p", "-b:v", "3M", "-g", "12", "-bf", "2", "-s", "640x360", "-r", "25"];
    let a = ["-c:a", "mp2", "-b:a", "128k", "-ac", "2", "-ar", "48000"];
    let clip = make_av("pb_mpeg2_mp2.mpg", &v, &a, &["-f", "mpeg"]);
    let time = Arc::new(FakeTime::new());
    let sink = Arc::new(FakeSink { time: time.clone(), st: Mutex::new(St::default()) });
    let mut pb = match Playback::open_with_sink(&clip, 6, Some(sink.clone())) {
        Ok(p) => p,
        Err(rustdecoder::Error::Gpu(m)) => {
            eprintln!("skipped: {m}");
            return;
        }
        Err(e) => panic!("open: {e}"),
    };
    if !pb.has_audio() {
        eprintln!("skipped: no MP2 decoder ({})", pb.audio_note());
        return;
    }
    pb.play();
    let mut last_serial = 0;
    let mut last_pts = -1.0;
    let mut shown = 0;
    for _ in 0..90 {
        let Some(f) = step(&mut pb, &time) else { continue };
        let now = pb.time();
        assert!(f.pts <= now + 1e-9, "a picture from the future: {} at {now}", f.pts);
        if f.serial != last_serial {
            assert!(f.pts > last_pts, "pictures out of order: {} after {last_pts}", f.pts);
            last_pts = f.pts;
            last_serial = f.serial;
            shown += 1;
        }
        let apos = pb.audio_position().unwrap();
        assert!((apos - now).abs() < 1e-6, "audio {apos} clock {now}");
    }
    let st = pb.stats();
    println!("1.5 s at 60 Hz: {shown} pictures, dropped {}", st.dropped);
    assert!((35..=39).contains(&shown), "{shown} pictures in 1.5 s of 25 fps");
    assert_eq!(st.dropped, 0);
    // the sound queued is one unbroken run (MP2 frames of 1152 samples)
    let mut next: Option<f64> = None;
    let mut total = 0usize;
    for e in &sink.st.lock().unwrap().log {
        match e {
            Ev::Push { pts, frames } => {
                if let Some(n) = next {
                    assert!((pts - n).abs() < 1e-6, "audio gap: block at {pts}, expected {n}");
                }
                next = Some(pts + *frames as f64 / 48000.0);
                total += frames;
            }
            Ev::Flush => panic!("flush without a seek"),
        }
    }
    println!("audio queued: {total} frames");
    assert!(next.unwrap_or(0.0) >= 1.5, "audio behind the clock");
    // seeks: the picture is at or just before the target
    for target in [1.0f64, 0.2, 0.0] {
        pb.seek(target);
        assert!((pb.time() - target).abs() < 1e-3, "clock {} after a seek to {target}", pb.time());
        let mut first = None;
        for _ in 0..40 {
            if let Some(f) = step(&mut pb, &time) {
                first = Some(f);
                break;
            }
        }
        let f = first.expect("no picture after the seek");
        let after = pb.time();
        println!("seek to {target}: first picture {:.4} (clock {after:.4})", f.pts);
        assert!(f.pts <= after + 1e-9 && after - f.pts <= 1.0 / 25.0 + 1e-6, "picture {} for clock {after}", f.pts);
    }
}
