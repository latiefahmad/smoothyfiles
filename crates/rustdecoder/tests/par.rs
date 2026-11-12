//! Pixel aspect ratio: read from the MP4 `pasp` box, the H.264 / HEVC VUI `sample_aspect_ratio`
//! and the Matroska `DisplayWidth` / `DisplayHeight`, put into `StreamInfo::display`, and used by
//! the window's letterbox. Clips are 640x360 test patterns made with ffmpeg (`-vf setsar=4/3`,
//! `-aspect`): 4/3 pixels are 853x360 on show. Tests skip with a message when ffmpeg or a GPU
//! decoder is missing.

#![cfg(windows)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{have_ffmpeg, temp_dir};
use rustdecoder::VideoDecoder;
use rustdecoder::window::{Placement, VideoWindow, WindowOptions, letterbox};

/// Makes (once) a 1 s clip with `vf` / `extra` applied; the extension picks the container.
fn make(name: &str, codec: &[&str], vf: &str, extra: &[&str]) -> PathBuf {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let out = temp_dir().join(name);
    if out.exists() {
        return out;
    }
    let tmp = out.with_file_name(format!("part-{name}"));
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30", "-t", "1", "-vf", vf])
        .args(codec)
        .args(["-pix_fmt", "yuv420p"])
        .args(extra)
        .arg(&tmp)
        .status()
        .expect("ffmpeg");
    assert!(status.success(), "ffmpeg could not make {name}");
    std::fs::rename(&tmp, &out).unwrap();
    out
}

const H264: [&str; 4] = ["-c:v", "libx264", "-x264-params", "log-level=error"];
const HEVC: [&str; 4] = ["-c:v", "libx265", "-x265-params", "log-level=error"];

/// A copy of `clip` whose `pasp` box has been renamed `free`: the stream's own VUI is all that
/// is left to say what the pixels are.
fn without_pasp(clip: &Path, name: &str) -> PathBuf {
    let out = temp_dir().join(name);
    let mut b = std::fs::read(clip).unwrap();
    let at = b.windows(4).position(|w| w == b"pasp").expect("the clip has no pasp box");
    b[at..at + 4].copy_from_slice(b"free");
    std::fs::write(&out, b).unwrap();
    out
}

/// `(container's pixel aspect, display size)`; `None` when the GPU cannot decode the clip.
fn read(clip: &Path) -> Option<(Option<(u32, u32)>, (u32, u32))> {
    let movie = rustdecoder::container::read(&mut std::fs::File::open(clip).unwrap()).unwrap();
    let par = movie.video().unwrap().par;
    match VideoDecoder::open(clip) {
        Ok(d) => {
            let i = d.info();
            assert_eq!((i.width, i.height), (640, 360));
            Some((par, i.display))
        }
        Err(rustdecoder::Error::Gpu(m)) => {
            eprintln!("skipped: {m}");
            None
        }
        Err(e) => panic!("open {}: {e}", clip.display()),
    }
}

#[test]
fn square_pixels_display_the_coded_size() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let clip = make("par_square.mp4", &H264, "null", &[]);
    let Some((par, display)) = read(&clip) else { return };
    // ffmpeg writes no `pasp` for square pixels
    assert!(par.is_none() || par == Some((1, 1)), "{par:?}");
    assert_eq!(display, (640, 360));
}

#[test]
fn h264_pasp_and_vui() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let clip = make("par_h264_43.mp4", &H264, "setsar=4/3", &[]);
    let Some((par, display)) = read(&clip) else { return };
    assert_eq!(par, Some((4, 3)), "pasp");
    assert_eq!(display, (853, 360));
    // the same clip with the box taken away: the SPS VUI says it
    let bare = without_pasp(&clip, "par_h264_43_nopasp.mp4");
    let Some((par, display)) = read(&bare) else { return };
    assert_eq!(par, None);
    assert_eq!(display, (853, 360), "from the H.264 VUI");
    // a PAL pixel (aspect_ratio_idc 2, 12:11)
    let pal = make("par_h264_1211.mp4", &H264, "setsar=12/11", &[]);
    let bare = without_pasp(&pal, "par_h264_1211_nopasp.mp4");
    assert_eq!(read(&bare).map(|r| r.1), Some((698, 360)), "12:11 from the table");
}

#[test]
fn hevc_pasp_and_vui() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let clip = make("par_hevc_43.mp4", &HEVC, "setsar=4/3", &["-tag:v", "hvc1"]);
    let Some((par, display)) = read(&clip) else { return };
    assert_eq!(par, Some((4, 3)), "pasp");
    assert_eq!(display, (853, 360));
    let bare = without_pasp(&clip, "par_hevc_43_nopasp.mp4");
    let Some((par, display)) = read(&bare) else { return };
    assert_eq!(par, None);
    assert_eq!(display, (853, 360), "from the HEVC VUI");
}

#[test]
fn matroska_display_size() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    // 4/3 pixels
    let clip = make("par_h264_43.mkv", &H264, "setsar=4/3", &[]);
    let Some((par, display)) = read(&clip) else { return };
    // (ffmpeg writes a DisplayWidth / DisplayHeight pair that is 4:3 pixels of 640x360)
    assert_eq!(par, Some((4, 3)), "DisplayWidth / DisplayHeight");
    assert_eq!(display, (853, 360));
    // a display aspect of 2:1 on a 16:9 picture
    let wide = make("par_hevc_21.mkv", &HEVC, "setsar=9/8", &[]);
    let Some((_, display)) = read(&wide) else { return };
    assert_eq!(display, (720, 360));
    // VP9 in WebM has no codec VUI: the container's size is all there is
    let webm = make("par_vp9_43.webm", &["-c:v", "libvpx-vp9", "-b:v", "1M"], "setsar=4/3", &[]);
    let Some((par, display)) = read(&webm) else { return };
    assert!(par.is_some());
    assert_eq!(display, (853, 360));
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The window letterboxes the display aspect: 853x360 pictures in 800x600 take rows 131..469.
#[test]
fn the_window_letterboxes_by_the_display_aspect() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let clip = make("par_h264_43.mp4", &H264, "setsar=4/3", &[]);
    let bg = [0.0, 0.5, 1.0, 1.0];
    let opts = WindowOptions { background: bg, audio: false, visible: false, autoplay: false, ..Default::default() };
    let win = match VideoWindow::open(&clip, Placement::TopLevel { title: "rustdecoder par".into(), width: 800, height: 600 }, opts) {
        Ok(w) => w,
        Err(rustdecoder::Error::Gpu(m)) => {
            eprintln!("skipped: {m}");
            return;
        }
        Err(e) => panic!("open: {e}"),
    };
    wait_for("the first picture", || win.state().picture.is_some() && win.state().presented >= 2);
    let st = win.state();
    assert_eq!(st.video_size, (853, 360), "the size on show");
    assert_eq!(st.size, (800, 600));
    let want = letterbox(800, 600, 853, 360);
    assert_eq!(want, (0, 131, 800, 338));
    let c = win.capture().unwrap();
    let bg8 = [0u8, 127, 255];
    let is_bg = |y: u32| {
        let m = c.mean(0, y, 800, 1);
        (0..3).all(|k| (m[k] - bg8[k] as f64).abs() < 1.5)
    };
    // bars above and below, the picture right up to the edge of the letterbox rectangle
    assert!(is_bg(0) && is_bg(100) && is_bg(129) && is_bg(130), "bar above");
    assert!(is_bg(470) && is_bg(500) && is_bg(599), "bar below");
    for y in [131, 132, 300, 467, 468] {
        assert!(!is_bg(y), "row {y} should be picture");
    }
    eprintln!("853x360 in 800x600: picture rows {}..{}", want.1, want.1 + want.3 as i32);
}
