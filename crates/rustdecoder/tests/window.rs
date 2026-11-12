//! Presenting into a window, on a hidden top-level window (nothing appears on screen): the back
//! buffer is read back and checked for the letterbox, the background colour and the picture; a
//! paused window presents nothing; a seek and a resize reach the swap chain. Tests skip with a
//! message when ffmpeg or a GPU decoder is missing. No sound: the audio is switched off.

#![cfg(windows)]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::*;
use rustdecoder::window::{Placement, VideoWindow, WindowEvent, WindowOptions};

const BG: [f32; 4] = [0.0, 0.5, 1.0, 1.0];

fn open(clip: &Path, w: u32, h: u32) -> Option<VideoWindow> {
    let opts = WindowOptions { background: BG, audio: false, visible: false, autoplay: true, ..Default::default() };
    match VideoWindow::open(clip, Placement::TopLevel { title: "rustdecoder test".into(), width: w, height: h }, opts) {
        Ok(v) => Some(v),
        Err(rustdecoder::Error::Gpu(m)) => {
            eprintln!("skipped: {m}");
            None
        }
        Err(e) => panic!("open: {e}"),
    }
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn resize(win: &VideoWindow, w: i32, h: i32) {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let hwnd = HWND(win.hwnd() as *mut _);
        // a client size of w x h
        let mut r = RECT { left: 0, top: 0, right: w, bottom: h };
        let _ = AdjustWindowRectEx(&mut r, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE(0));
        let _ = SetWindowPos(hwnd, None, 0, 0, r.right - r.left, r.bottom - r.top, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// The background as the GPU writes it (0.5 may become 127 or 128).
fn bg8() -> [u8; 3] {
    [(BG[0] * 255.0) as u8, (BG[1] * 255.0) as u8, (BG[2] * 255.0) as u8]
}

fn run(clip: &Path) {
    let Some(win) = open(clip, 640, 480) else { return };
    wait_for("the first pictures", || win.state().picture.is_some() && win.state().presented >= 3);
    let st = win.state();
    assert_eq!(st.size, (640, 480), "client size");
    assert_eq!(st.video_size, (640, 360));
    eprintln!("{} {}x{} in {}x{}: presented {}", st.codec, st.video_size.0, st.video_size.1, st.size.0, st.size.1, st.presented);

    // 16:9 in 4:3: 60 px bars above and below, the background colour, the picture between
    let c = win.capture().unwrap();
    assert_eq!((c.width, c.height), (640, 480));
    for (x, y) in [(0, 0), (639, 0), (320, 30), (0, 59), (639, 479), (320, 450), (10, 420 + 1)] {
        assert_eq!(&c.pixel(x, y)[..3], &bg8(), "bar pixel at {x},{y}");
    }
    let picture_rows = (60..420).map(|y| c.mean(0, y, 640, 1)).collect::<Vec<_>>();
    assert!(picture_rows.iter().all(|m| m != &[bg8()[0] as f64, bg8()[1] as f64, bg8()[2] as f64]), "the bars reach into the picture");
    let mid = c.mean(300, 200, 40, 40);
    assert!(mid.iter().any(|&v| v > 5.0), "the picture is black: {mid:?}");

    // paused: no presents at all
    win.pause();
    wait_for("pause", || !win.state().playing);
    std::thread::sleep(Duration::from_millis(150));
    let before = win.state().presented;
    std::thread::sleep(Duration::from_millis(600));
    let after = win.state().presented;
    eprintln!("paused for 0.6 s: {} presents", after - before);
    assert_eq!(before, after, "a paused window kept presenting");

    // a seek while paused lands on a new picture, and exactly one present follows
    let was = win.state().picture;
    win.seek(1.5);
    wait_for("the picture after the seek", || win.state().picture.is_some_and(|p| (p - 1.5).abs() < 0.04));
    std::thread::sleep(Duration::from_millis(100));
    let n = win.state().presented;
    assert!(n >= after + 1 && n <= after + 3, "presents for one seek: {}", n - after);
    assert_ne!(was, win.state().picture);
    let c2 = win.capture().unwrap();
    assert_ne!(c.bgra, c2.bgra, "the picture did not change with the seek");

    // a resize reaches the swap chain, the picture follows the aspect
    resize(&win, 300, 600);
    wait_for("the resize", || win.state().size == (300, 600));
    let c3 = win.capture().unwrap();
    assert_eq!((c3.width, c3.height), (300, 600));
    // 16:9 in a tall window: 169 px picture band in the middle (rows 215..385)
    assert_eq!(&c3.pixel(150, 100)[..3], &bg8());
    assert_eq!(&c3.pixel(150, 500)[..3], &bg8());
    assert_ne!(&c3.pixel(150, 300)[..3], &bg8());
    eprintln!("resized to 300x600: capture {}x{}, bars and picture as expected", c3.width, c3.height);

    // play again from the pause: pictures keep coming
    let p0 = win.state().presented;
    win.play();
    wait_for("playing", || win.state().presented >= p0 + 10);
    // nothing touched the window: no events
    assert!(win.take_events().is_empty());
    drop(win);
}

#[test]
fn hevc_into_a_hidden_window() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    run(&make_clip("w_hevc.mp4", "yuv420p", "bframes=2:keyint=30", false));
}

#[test]
fn h264_into_a_hidden_window() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    run(&make_h264_clip("w_h264.mp4", "bframes=3:keyint=30:scenecut=0", false));
}

#[test]
fn volume_mute_and_end_of_stream() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let clip = make_h264_clip("w_h264_end.mp4", "bframes=2:keyint=30", false);
    let Some(win) = open(&clip, 320, 180) else { return };
    // without a sound device in use these are no-ops, but they must not disturb anything
    win.set_volume(0.3);
    win.set_muted(true);
    wait_for("the end", || win.state().ended);
    let st = win.state();
    eprintln!("ended at {:.2} of {:.2} s, {} shown, {} dropped", st.position, st.duration, st.shown, st.dropped);
    assert!(!st.playing);
    assert!((st.position - st.duration).abs() < 0.05);
    assert!(st.shown >= 85, "{} pictures shown of 90", st.shown);
    // playing again starts from the beginning
    win.play();
    wait_for("the restart", || win.state().playing && !win.state().ended);
    assert!(win.state().position < 1.5);
    let _ = WindowEvent::Click;
}

#[test]
fn mouse_messages_become_events() {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::Controls::WM_MOUSELEAVE;
    use windows::Win32::UI::WindowsAndMessaging::*;
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let clip = make_h264_clip("w_h264_events.mp4", "bframes=2:keyint=30", false);
    let Some(win) = open(&clip, 320, 180) else { return };
    let hwnd = HWND(win.hwnd() as *mut _);
    let post = |m: u32, wp: usize| unsafe {
        let _ = PostMessageW(Some(hwnd), m, WPARAM(wp), LPARAM((20 << 16) | 30));
    };
    post(WM_MOUSEMOVE, 0);
    post(WM_MOUSEMOVE, 0);
    post(WM_MOUSEMOVE, 0);
    post(WM_LBUTTONDOWN, 1);
    post(WM_LBUTTONDBLCLK, 1);
    post(WM_MOUSEWHEEL, (120u16 as usize) << 16);
    post(WM_MOUSELEAVE, 0);
    let mut got = Vec::new();
    wait_for("the events", || {
        got.extend(win.take_events());
        got.iter().position(|e| *e == WindowEvent::Wheel(120)).is_some_and(|i| got[i..].contains(&WindowEvent::Leave))
    });
    // the three moves are one event (queued before anybody takes them; if the reader ran in
    // between there can be two)
    let moves = got.iter().filter(|e| **e == WindowEvent::MouseMove).count();
    assert!((1..=2).contains(&moves), "{got:?}");
    // (the system itself reports a leave at once: the pointer is not over a hidden window)
    assert_eq!(got.last(), Some(&WindowEvent::Leave));
    got.retain(|e| *e != WindowEvent::Leave);
    got.dedup();
    assert_eq!(got, [WindowEvent::MouseMove, WindowEvent::Click, WindowEvent::DoubleClick, WindowEvent::Wheel(120)]);
}
