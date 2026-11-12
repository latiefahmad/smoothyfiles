use std::path::PathBuf;
use std::time::{Duration, Instant};

use rf_player::{AudioSink, Clock, NullSink, Player, open};

fn data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

fn wait_until(mut f: impl FnMut() -> bool, ms: u64) {
    let t = Instant::now();
    while !f() && t.elapsed() < Duration::from_millis(ms) {
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn clock_follows_audio_sink() {
    let mut s = NullSink::new(48000, 2);
    s.advance_frames(24000);
    assert!((Clock::audio(&s).now() - 0.5).abs() < 1e-6);
}

#[test]
fn null_sink_does_not_play_ahead_of_what_was_written() {
    let mut s = NullSink::new(48000, 2);
    s.pause(false);
    let n = s.write(&vec![0.0; 480 * 2]);
    assert_eq!(n, 480);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(s.played_frames(), 480);
}

#[test]
fn plays_to_end_and_reports() {
    let mut p = Player::new(open(&data("h264.mp4")).unwrap(), Box::new(NullSink::new(48000, 2)));
    p.play();
    wait_until(|| p.state().ended, 3000);
    assert!(p.state().ended);
    assert!(p.state().position >= 0.95);
}

#[test]
fn audio_file_plays_on_audio_clock() {
    let mut p = Player::new(open(&data("silent.mp3")).unwrap(), Box::new(NullSink::new(48000, 2)));
    p.play();
    std::thread::sleep(Duration::from_millis(300));
    let pos = p.state().position;
    assert!(pos > 0.1 && pos < 0.6, "position {pos}");
}

#[test]
fn seek_shows_right_frame() {
    let mut p = Player::new(open(&data("h264.mp4")).unwrap(), Box::new(NullSink::new(48000, 2)));
    p.seek(0.5);
    wait_until(|| p.current_frame().is_some_and(|f| (f.pts - 0.5).abs() < 0.05), 1000);
    assert!((p.current_frame().unwrap().pts - 0.5).abs() < 0.05);
}

#[test]
fn looping_restarts() {
    let mut p = Player::new(open(&data("h264.mp4")).unwrap(), Box::new(NullSink::new(48000, 2)));
    p.set_loop(true);
    p.seek(0.9);
    p.play();
    std::thread::sleep(Duration::from_millis(400));
    let s = p.state();
    assert!(!s.ended && s.playing && s.position < 0.5, "{s:?}");
}
