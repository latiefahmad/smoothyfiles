//! DirectLook's rustdecoder route (opt-in: `SMOOTHYFILES_RUSTDECODER=1`): MP4 / MOV files with H.264
//! or HEVC play through `rustdecoder` (own container parser, GPU video decoder, WASAPI sound,
//! flip-model swap chain) in a popup window owned by the viewer window and placed over the video
//! area, the way the Media Foundation route does it. Whatever rustdecoder cannot open falls back
//! to the routes `video.rs` has always used.

use std::path::Path;

use rustdecoder::mp4::Codec;
use rustdecoder::window::{Placement, VideoWindow, WindowOptions, WindowState};

/// The route is on only when `SMOOTHYFILES_RUSTDECODER=1`.
pub fn enabled() -> bool {
    std::env::var("SMOOTHYFILES_RUSTDECODER").is_ok_and(|v| v == "1")
}

/// What the container says about a file rustdecoder might play.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Probe {
    /// Size as displayed (after the rotation).
    pub width: u32,
    pub height: u32,
    pub codec: &'static str,
}

/// Reads the container's index (no GPU): `Some` for an MP4 / MOV / MKV / WebM / MPEG-TS / PS / AVI / MXF whose first video
/// track is H.264, HEVC, VP9, AV1, ProRes, DNxHD / DNxHR or MPEG-2. Whether the stream itself is playable (4:2:0, 8 or 10 bit, a decoder in the graphics
/// card) is only known when the decoder opens; [`RdVideo::open`] fails then.
pub fn probe(path: &Path) -> Option<Probe> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if !matches!(ext.as_str(), "mp4" | "mov" | "m4v" | "mkv" | "webm" | "ts" | "m2ts" | "mts" | "avi" | "mxf" | "mpg" | "mpeg" | "vob" | "m2v") {
        return None;
    }
    let movie = rustdecoder::container::read(&mut std::fs::File::open(path).ok()?).ok()?;
    let t = movie.video()?;
    let codec = match t.codec {
        Codec::H264 { .. } => "H.264",
        Codec::Hevc { .. } => "HEVC",
        Codec::Vp9 { .. } => "VP9",
        Codec::Av1 { .. } => "AV1",
        _ if rustdecoder::prores::is_prores_fourcc(&t.fourcc) => "ProRes",
        _ if rustdecoder::dnx::is_dnx_fourcc(&t.fourcc) => "DNxHR",
        _ if &t.fourcc == b"mpg2" => "MPEG-2",
        _ => return None,
    };
    let (w, h) = rustdecoder::present::rotated_size(t.width, t.height, t.rotation);
    Some(Probe { width: w, height: h, codec })
}

/// What a playing rustdecoder window needs from the viewer.
pub struct Settings {
    /// 0..1.
    pub volume: f32,
    pub muted: bool,
    pub looping: bool,
}

/// A playing video in its popup. Dropping hides the window at once and takes it down on another
/// thread (joining the render thread can take a few milliseconds).
pub struct RdVideo {
    win: Option<VideoWindow>,
}

impl RdVideo {
    /// Opens `path` into a popup owned by `owner` (an HWND) at `rect` (left, top, right, bottom,
    /// screen pixels). Blocks while the decoder and the audio device start: run it off the UI
    /// thread. Fails when rustdecoder cannot play the file.
    pub fn open(path: &Path, owner: isize, rect: [i32; 4], s: &Settings) -> Result<RdVideo, String> {
        let place = Placement::Popup { owner, x: rect[0], y: rect[1], width: (rect[2] - rect[0]).max(1) as u32, height: (rect[3] - rect[1]).max(1) as u32 };
        let muted = s.muted || std::env::var_os("SMOOTHYFILES_MUTE").is_some();
        let opts = WindowOptions { background: [0.0, 0.0, 0.0, 1.0], audio: true, muted, volume: s.volume, looping: s.looping, autoplay: true, visible: true, ring: 6 };
        let win = VideoWindow::open(path, place, opts).map_err(|e| e.to_string())?;
        Ok(RdVideo { win: Some(win) })
    }

    fn w(&self) -> &VideoWindow {
        self.win.as_ref().expect("window present until drop")
    }

    pub fn state(&self) -> WindowState {
        self.w().state()
    }

    pub fn toggle(&self) {
        self.w().toggle();
    }

    pub fn seek_fraction(&self, f: f64) {
        self.w().seek_fraction(f);
    }

    pub fn set_volume(&self, v: f32) {
        self.w().set_volume(v);
    }

    pub fn set_muted(&self, m: bool) {
        self.w().set_muted(m);
    }

    pub fn set_looping(&self, l: bool) {
        self.w().set_looping(l);
    }

    /// Follows the viewer window: `rect` as in [`RdVideo::open`].
    pub fn reposition(&self, rect: [i32; 4]) {
        self.w().set_rect(rect[0], rect[1], (rect[2] - rect[0]).max(1) as u32, (rect[3] - rect[1]).max(1) as u32);
    }

    pub fn take_events(&self) -> Vec<rustdecoder::window::WindowEvent> {
        self.w().take_events()
    }
}

impl Drop for RdVideo {
    fn drop(&mut self) {
        if let Some(w) = self.win.take() {
            w.set_visible(false);
            let _ = std::thread::Builder::new().name("rd-video-teardown".into()).spawn(move || drop(w));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_by_default() {
        // the variable is not set in the test environment unless somebody asked for it
        if std::env::var_os("SMOOTHYFILES_RUSTDECODER").is_none() {
            assert!(!enabled());
        }
    }

    #[test]
    fn other_containers_are_not_probed() {
        assert_eq!(probe(Path::new("clip.mkv")), None);
        assert_eq!(probe(Path::new("clip.avi")), None);
        assert_eq!(probe(Path::new("no-such-file.mp4")), None);
        assert_eq!(probe(Path::new("noextension")), None);
    }
}
