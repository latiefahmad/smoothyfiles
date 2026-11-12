//! Video ve ses görüntüleyici (rf-player: FilmCraft çözücüleri, WASAPI ses).
//! Video: kareler pencere boyutuna ölçeklenip arka planda BGRA'ya çevrilir; başlık çubuğu ve
//! kontroller içeriğin üstünde durur, fare durunca gizlenir. Tıklama oynat/duraklat, çift tık
//! tam ekran, tekerlek ses düzeyi (0,04). Ses: 500×300 kart (kapak, başlık/sanatçı/albüm,
//! süre, eşzamanlı şarkı sözü). FilmCraft'ın açamadığı dosyalar Media Foundation ile oynatılır.
//! İsteğe bağlı: SMOOTHYFILES_RUSTDECODER=1 ile MP4/MOV (H.264, HEVC) kendi motorumuzla (rd_video.rs) oynatılır;
//! açılamayan dosyalar yukarıdaki yollara düşer.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as L;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rf_player::{AudioSink, Player, PlayerState};

use super::rd_video;
use crate::look::viewer::{Prepared, Probe, Viewer};
use crate::look::config::LookSettings;
use crate::t;

/// Kontroller fare durunca bu kadar sonra gizlenir.
const HIDE_AFTER: Duration = Duration::from_millis(1500);
/// Oynatma döngüsünün yoklama aralığı.
const TICK: Duration = Duration::from_millis(8);
/// While the timeline is dragged the player is sent a new position at most this often (every
/// jump makes it decode from a key frame; the bar itself follows the pointer at once).
const SCRUB_EVERY: Duration = Duration::from_millis(80);
/// After a jump the bar stays on its target this long at most, until the player reports it (the
/// Media Foundation and rustdecoder routes report their position with a delay).
const SEEK_HOLD: Duration = Duration::from_millis(800);

/// The timeline being dragged (GPUI's drag: its moves arrive wherever the pointer goes).
struct SeekDrag;

/// Nothing follows the pointer while the timeline is dragged.
struct SeekGhost;

impl Render for SeekGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[derive(Default)]
pub struct VideoViewer {
    view: Option<Entity<MediaView>>,
    /// The file opened off the UI thread (a network drive), see `Viewer::loader`.
    opened: Option<Opened>,
}

/// The route a file plays by and the window it asks for.
struct Opened(Kind, Prepared);

/// Opens `path`: reads the container's index (on a network drive this is the slow part) and
/// picks the route it plays by.
fn open_media(path: &std::path::Path) -> Opened {
    let name = super::file_name(path);
    let opened = rf_player::open(path);
    let (kind, prepared) = match opened {
        Ok(media) => match media.info().video.clone() {
            // FilmCraft decodes on the CPU: 4K, or anything past 1080p60, doesn't keep up (4K60
            // HEVC 10-bit: 42 frames/s on a 9800X3D with nothing else running). Windows'
            // decoder uses the graphics card's video engine instead.
            Some(v) if too_heavy(v.width, v.height, v.frame_rate.as_f64()) && rf_win::preview_host::is_playable(path) && rf_win::preview_host::mf_decodes(&v.codec) => {
                log::info!("look: {}x{} {} at {:.2} fps is past software decoding, Media Foundation (hardware)", v.width, v.height, v.codec, v.frame_rate.as_f64());
                (Kind::Mf, Prepared::new((960.0, 540.0), name))
            }
            Some(v) => {
                let par = if v.par.1 > 0 { v.par.0 as f32 / v.par.1 as f32 } else { 1.0 };
                let size = ((v.width as f32 * par).max(160.0), (v.height as f32).max(90.0));
                let mut p = Prepared::new(size, name);
                p.overlay_titlebar = true;
                p.drag_by_body = false;
                (Kind::Video(media), p)
            }
            None => {
                let mut p = Prepared::new((500.0, 300.0), name);
                p.resizable = false;
                (Kind::Audio(media), p)
            }
        },
        Err(e) if rf_win::preview_host::is_playable(path) => {
            log::info!("look: rf-player cannot open {}, Media Foundation: {e:#}", path.display());
            (Kind::Mf, Prepared::new((960.0, 540.0), name))
        }
        Err(e) => (Kind::Failed(format!("{e:#}")), Prepared::new((520.0, 220.0), name)),
    };
    // opt-in: rustdecoder plays MP4 / MOV with H.264 or HEVC; what it cannot open falls back to
    // `kind` (decided when its decoder opens, see MediaView::rd_opened)
    match kind {
        Kind::Video(_) | Kind::Mf if rd_video::enabled() => match rd_video::probe(path) {
            Some(p) => {
                log::info!("look: rustdecoder route for {}: {} {}x{}", path.display(), p.codec, p.width, p.height);
                Opened(Kind::Rd(Box::new(kind), p), Prepared::new((960.0, 540.0), super::file_name(path)))
            }
            None => {
                log::info!("look: rustdecoder does not take {}, current route", path.display());
                Opened(kind, prepared)
            }
        },
        kind => Opened(kind, prepared),
    }
}

impl Viewer for VideoViewer {
    fn name(&self) -> &'static str {
        "video"
    }
    fn priority(&self) -> i32 {
        -3
    }
    fn can_handle(&self, f: &Probe) -> bool {
        // `.ts` çoğunlukla TypeScript: yalnız MPEG-TS paket eşitlemesi varsa video
        if f.ext == "ts" && !is_transport_stream(&f.head) {
            return false;
        }
        f.is_file() && (rf_player::can_open(&f.path) || rf_win::preview_host::is_playable(&f.path))
    }
    fn loader(&self) -> Option<fn(&Probe) -> crate::look::viewer::Loaded> {
        Some(|f| Box::new(open_media(&f.path)))
    }
    fn take_loaded(&mut self, loaded: crate::look::viewer::Loaded) {
        self.opened = loaded.downcast::<Opened>().ok().map(|o| *o);
    }
    fn prepare(&mut self, f: &Probe, cx: &mut App) -> Prepared {
        // yalnız dizin okunur (hızlı); ilk karenin çözülebildiği arka planda denenir (MediaView::new)
        let Opened(kind, prepared) = self.opened.take().unwrap_or_else(|| open_media(&f.path));
        let path = f.path.clone();
        self.view = Some(cx.new(|cx| MediaView::new(path, kind, cx)));
        prepared
    }
    fn render(&mut self, _window: &mut Window, _cx: &mut App) -> AnyElement {
        self.view.as_ref().map_or_else(|| div().into_any_element(), |v| v.clone().into_any_element())
    }
    fn cleanup(&mut self) {
        self.view = None;
    }
}

/// Pixels per second past which the CPU decoder can't be trusted to play in real time
/// (1080p at 60 fps; 4K at any rate is past it).
const SOFTWARE_PIXEL_RATE: f64 = 1920.0 * 1080.0 * 60.0 * 1.05;

fn too_heavy(width: u32, height: u32, fps: f64) -> bool {
    let fps = if fps > 0.0 { fps } else { 30.0 };
    width as f64 * height as f64 * fps > SOFTWARE_PIXEL_RATE
}

#[cfg(test)]
mod heavy_tests {
    #[test]
    fn past_1080p60_goes_to_hardware() {
        assert!(!super::too_heavy(1920, 1080, 60000.0 / 1001.0));
        assert!(!super::too_heavy(1920, 1080, 60.0));
        assert!(super::too_heavy(3840, 2160, 24.0));
        assert!(super::too_heavy(2160, 3840, 60000.0 / 1001.0));
        assert!(super::too_heavy(2560, 1440, 60.0));
        assert!(!super::too_heavy(1280, 720, 0.0));
    }
}

/// MPEG-TS: 188 baytlık (ya da 4 bayt zaman damgalı 192 baytlık) paketlerin başında 0x47.
pub(crate) fn is_transport_stream(head: &[u8]) -> bool {
    let synced = |start: usize, step: usize| (0..3).all(|k| head.get(start + k * step) == Some(&0x47));
    synced(0, 188) || synced(4, 192)
}

enum Kind {
    Video(rf_player::Media),
    Audio(rf_player::Media),
    Mf,
    /// rustdecoder first; the boxed kind is the fallback.
    Rd(Box<Kind>, rd_video::Probe),
    Failed(String),
}

fn sink() -> Box<dyn AudioSink> {
    if std::env::var_os("SMOOTHYFILES_MUTE").is_some() {
        return Box::new(rf_player::NullSink::new(48000, 2));
    }
    rf_player::wasapi_sink().unwrap_or_else(|e| {
        log::warn!("look audio: {e:#}");
        Box::new(rf_player::NullSink::new(48000, 2))
    })
}

fn clock(t: f64) -> String {
    let t = t.max(0.0) as u64;
    if t >= 3600 { format!("{}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60) } else { format!("{}:{:02}", t / 60, t % 60) }
}

fn bgra_image(w: u32, h: u32, data: Vec<u8>) -> Option<Arc<RenderImage>> {
    let buf = image::RgbaImage::from_raw(w, h, data)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buf)])))
}

pub struct MediaView {
    path: PathBuf,
    video: bool,
    mf: bool,
    failed: Option<String>,
    player: Option<Player>,
    state: PlayerState,
    image: Option<Arc<RenderImage>>,
    /// Bir önceki kare: GPU dokusu bir kare sonra bırakılır (o an çizimde olabilir).
    previous: Option<Arc<RenderImage>>,
    shown_pts: f64,
    converting: bool,
    /// Kare çevrilecek boyut (aygıt pikseli).
    target: (u32, u32),
    volume: f32,
    muted: bool,
    looping: bool,
    controls: bool,
    hovered_controls: bool,
    bar: Rc<Cell<Option<Bounds<Pixels>>>>,
    meta: Vec<(&'static str, String)>,
    cover: Option<Arc<RenderImage>>,
    images: Option<Entity<crate::image_cache::ImageCache>>,
    lyrics: Vec<(f64, String)>,
    opened_at: Instant,
    first_frame_logged: bool,
    mf_rect: Option<[i32; 4]>,
    mf_owner: isize,
    /// Görüntü boyutu (piksel en-boyu uygulanmış).
    dims: (f32, f32),
    mf_state: Option<rf_win::preview_host::MediaState>,
    /// rustdecoder route chosen (its window may still be opening).
    rd_mode: bool,
    rd: Option<rd_video::RdVideo>,
    /// What to play when rustdecoder cannot open the file.
    rd_fallback: Option<Kind>,
    rd_opening: bool,
    rd_rect: Option<[i32; 4]>,
    rd_state: Option<rustdecoder::window::WindowState>,
    /// The video's own window reported a double click: the next render toggles fullscreen (it
    /// has the window).
    want_fullscreen: bool,
    /// The timeline is being dragged: the fraction under the pointer, which the bar shows
    /// instead of the player's position.
    scrub: Option<f64>,
    /// The fraction last sent to the player during the drag, and when.
    scrub_sent: Option<(f64, Instant)>,
    /// Playback was paused for the drag and goes on when it ends.
    scrub_resume: bool,
    /// The last jump's target and time: the bar stays there until the player reports it.
    seek_hold: Option<(f64, Instant)>,
    /// How long Media Foundation gets to open the file before it counts as unsupported.
    mf_wait: Duration,
    _hide: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
}

impl MediaView {
    fn new(path: PathBuf, kind: Kind, cx: &mut Context<Self>) -> Self {
        let s = cx.global::<LookSettings>();
        let (volume, looping) = (s.look_volume.clamp(0.0, 1.0), s.look_loop);
        let mut v = Self {
            path: path.clone(),
            video: false,
            mf: false,
            failed: None,
            player: None,
            state: PlayerState::default(),
            image: None,
            previous: None,
            shown_pts: f64::NAN,
            converting: false,
            target: (0, 0),
            volume,
            muted: false,
            looping,
            controls: true,
            hovered_controls: false,
            bar: Default::default(),
            meta: Vec::new(),
            cover: None,
            images: None,
            lyrics: Vec::new(),
            opened_at: Instant::now(),
            first_frame_logged: false,
            mf_rect: None,
            mf_owner: 0,
            dims: (16.0, 9.0),
            mf_state: None,
            rd_mode: false,
            rd: None,
            rd_fallback: None,
            rd_opening: false,
            rd_rect: None,
            rd_state: None,
            want_fullscreen: false,
            scrub: None,
            scrub_sent: None,
            scrub_resume: false,
            seek_hold: None,
            // a network drive needs longer than a local disk to hand over the file's header
            mf_wait: if rf_win::drives::slow_to_read(&path) { Duration::from_secs(20) } else { Duration::from_secs(2) },
            _hide: None,
            _tasks: Vec::new(),
        };
        v.start_kind(kind, cx);
        if !v.video && v.failed.is_none() {
            v.load_audio_info(cx);
        }
        // pencere kapanınca: ses durur, GPU dokuları bırakılır
        cx.on_release(|v: &mut Self, cx| {
            v.player = None;
            v.rd = None;
            if v.mf {
                rf_win::sta::run(|_| rf_win::preview_host::hide());
            }
            for img in [v.image.take(), v.previous.take(), v.cover.take()].into_iter().flatten() {
                cx.drop_image(img, None);
            }
        })
        .detach();
        v.arm_hide(cx);
        v
    }

    /// Starts what `kind` says: the rf-player route, Media Foundation, the audio card, or waits
    /// for rustdecoder's window (opened by the first `render`, which knows where it goes).
    fn start_kind(&mut self, kind: Kind, cx: &mut Context<Self>) {
        match kind {
            Kind::Video(media) | Kind::Audio(media) => {
                self.video = media.info().has_video();
                if let Some(i) = media.info().video.as_ref() {
                    let par = if i.par.1 > 0 { i.par.0 as f32 / i.par.1 as f32 } else { 1.0 };
                    self.dims = ((i.width as f32 * par).max(1.0), (i.height as f32).max(1.0));
                }
                if self.video {
                    self.probe_first_frame(media.clone(), cx);
                }
                let (volume, looping) = (self.volume, self.looping);
                let mut p = Player::new(media, sink());
                p.set_volume(volume);
                p.set_loop(looping);
                p.play();
                self.player = Some(p);
                self.tick(cx);
            }
            Kind::Mf => {
                self.video = true;
                self.start_mf(cx);
            }
            Kind::Rd(fallback, probe) => {
                self.rd_mode = true;
                self.video = true;
                self.controls = true;
                self.dims = (probe.width.max(1) as f32, probe.height.max(1) as f32);
                self.rd_fallback = Some(*fallback);
            }
            Kind::Failed(e) => self.failed = Some(e),
        }
    }

    /// Opens rustdecoder's popup over `rect` on a background thread.
    fn rd_open(&mut self, owner: isize, rect: [i32; 4], cx: &mut Context<Self>) {
        self.rd_opening = true;
        let path = self.path.clone();
        let s = rd_video::Settings { volume: self.volume, muted: self.muted, looping: self.looping };
        let job = cx.background_spawn(async move { rd_video::RdVideo::open(&path, owner, rect, &s) });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let r = job.await;
            // the viewer may be gone by now: the window then goes down with `r`
            let _ = this.update(cx, |v, cx| v.rd_opened(r, cx));
        }));
    }

    fn rd_opened(&mut self, r: Result<rd_video::RdVideo, String>, cx: &mut Context<Self>) {
        self.rd_opening = false;
        match r {
            Ok(rd) => {
                log::info!("look: rustdecoder opened {}", self.path.display());
                if let Some(rect) = self.rd_rect {
                    rd.reposition(rect);
                }
                self.rd = Some(rd);
                self.poll_rd(cx);
            }
            Err(e) => {
                log::info!("look: rustdecoder cannot play {}: {e}; falling back", self.path.display());
                self.rd_mode = false;
                self.rd_rect = None;
                if let Some(kind) = self.rd_fallback.take() {
                    self.opened_at = Instant::now();
                    self.start_kind(kind, cx);
                }
            }
        }
        cx.notify();
    }

    /// Takes the window's state and events (clicks, the wheel) into the controls.
    fn poll_rd(&mut self, cx: &mut Context<Self>) {
        self._tasks.push(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(80)).await;
            let alive = this.update(cx, |v, cx| {
                let Some(rd) = v.rd.as_ref() else { return false };
                let st = rd.state();
                let events = rd.take_events();
                let coarse = |s: &rustdecoder::window::WindowState| ((s.position * 10.0) as i64, s.playing, s.ended, (s.duration * 10.0) as i64);
                if v.rd_state.as_ref().is_none_or(|o| coarse(o) != coarse(&st)) {
                    cx.notify();
                }
                // the size on show (pixel aspect ratio and rotation applied) replaces what the
                // container index said, so the popup's rectangle has the picture's shape
                if st.video_size.0 > 0 && st.video_size.1 > 0 {
                    let shown = (st.video_size.0 as f32, st.video_size.1 as f32);
                    if shown != v.dims {
                        v.dims = shown;
                        cx.notify();
                    }
                }
                v.rd_state = Some(st);
                for e in events {
                    match e {
                        // the popup swallows the pointer, so what the viewer's own root does for the
                        // other routes (click plays, double click is fullscreen, the wheel is the
                        // volume, moving shows the controls) is done here from the popup's events
                        rustdecoder::window::WindowEvent::Click => v.toggle_play(cx),
                        // the second half of a double click: its first click's toggle is taken back
                        rustdecoder::window::WindowEvent::DoubleClick => {
                            v.toggle_play(cx);
                            v.want_fullscreen = true;
                            cx.notify();
                        }
                        rustdecoder::window::WindowEvent::Wheel(d) => {
                            v.set_volume(v.volume + 0.04 * d as f32 / 120.0, cx);
                            v.show_controls(cx);
                        }
                        rustdecoder::window::WindowEvent::MouseMove => v.show_controls(cx),
                        rustdecoder::window::WindowEvent::Leave => v.arm_hide(cx),
                        rustdecoder::window::WindowEvent::Closed => {}
                    }
                }
                true
            });
            if !alive.unwrap_or(false) {
                break;
            }
        }));
    }

    /// FilmCraft bazı biçimleri (ör. 4:2:2 HEVC) açar ama karesini çözemez: ilk kare arka planda
    /// denenir, olmazsa Media Foundation'a (o da yoksa açıklamaya) geçilir.
    fn probe_first_frame(&mut self, media: rf_player::Media, cx: &mut Context<Self>) {
        let probe = cx.background_spawn(async move { media.frame_at(0.0).err().map(|e| format!("{e:#}")) });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let Some(err) = probe.await else { return };
            let _ = this.update(cx, |v, cx| {
                v.player = None;
                if rf_win::preview_host::is_playable(&v.path) {
                    log::info!("look: rf-player cannot decode {}, Media Foundation: {err}", v.path.display());
                    v.opened_at = Instant::now();
                    v.start_mf(cx);
                } else {
                    v.failed = Some(err);
                }
                cx.notify();
            });
        }));
    }

    fn load_audio_info(&mut self, cx: &mut Context<Self>) {
        let p = self.path.clone();
        let work = cx.background_spawn(async move {
            let meta = rf_win::props::media_props(&p);
            let lyrics = crate::look::lyrics::sidecar(&p).unwrap_or_default();
            (meta, lyrics)
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let (meta, lyrics) = work.await;
            if std::env::var_os("SMOOTHYFILES_LOOK_TRACE").is_some() {
                eprintln!("look: ses bilgisi {meta:?}, {} söz satırı", lyrics.len());
            }
            let _ = this.update(cx, |v, cx| {
                v.meta = meta;
                v.lyrics = lyrics;
                cx.notify();
            });
        }));
        // kapak: kabuk küçük resmi, paylaşılan önbellekten (kendi iş parçacıklarında, mesaj döngülü)
        let images = crate::look::LookManager::images(cx);
        cx.observe(&images, |_, _, cx| cx.notify()).detach();
        self.images = Some(images);
    }

    /// Oynatma döngüsü: yeni kare varsa arka planda çevrilir; durum güncellenir.
    fn tick(&mut self, cx: &mut Context<Self>) {
        self._tasks.push(cx.spawn(async move |this, cx| {
            // paused or ended: a slower look (a click or a seek still shows within a few frames)
            let mut wait = TICK;
            loop {
                cx.background_executor().timer(wait).await;
                let job = this.update(cx, |v, cx| {
                    let p = v.player.as_ref()?;
                    let st = p.state();
                    wait = if st.playing { TICK } else { std::time::Duration::from_millis(33) };
                    let coarse = |s: &PlayerState| ((s.position * 10.0) as i64, s.playing, s.ended, (s.duration * 10.0) as i64);
                    if coarse(&st) != coarse(&v.state) {
                        cx.notify();
                    }
                    v.state = st;
                    if !v.video || v.converting || v.target.0 == 0 {
                        return None;
                    }
                    let f = p.current_frame()?;
                    if f.pts == v.shown_pts {
                        return None;
                    }
                    v.converting = true;
                    Some((f, v.target))
                });
                let Ok(job) = job else { break };
                let Some((frame, (w, h))) = job else { continue };
                let pts = frame.pts;
                let data = cx.background_spawn(async move { rf_player::to_bgra(&frame, w, h) }).await;
                let img = bgra_image(w, h, data);
                if this
                    .update(cx, |v, cx| {
                        v.converting = false;
                        v.shown_pts = pts;
                        if let Some(old) = v.previous.take() {
                            cx.drop_image(old, None);
                        }
                        v.previous = std::mem::replace(&mut v.image, img);
                        if !v.first_frame_logged {
                            v.first_frame_logged = true;
                            crate::look::trace::mark("ilk video karesi");
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    /// The Media Foundation route: Windows draws the video in a window of its own over ours.
    /// Clicks on the picture land there, so that window tells us what they did.
    fn start_mf(&mut self, cx: &mut Context<Self>) {
        self.mf = true;
        self.controls = true;
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        rf_win::preview_host::set_media_events(move |e| {
            let _ = tx.unbounded_send(e);
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            use rf_win::preview_host::MediaEvent;
            while let Some(e) = rx.next().await {
                let alive = this.update(cx, |v, cx| {
                    match e {
                        // the button follows at once (the state is otherwise polled)
                        MediaEvent::Playing(playing) => {
                            if let Some(s) = &mut v.mf_state {
                                s.playing = playing;
                            }
                        }
                        MediaEvent::DoubleClick => v.want_fullscreen = true,
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
        self.poll_mf(cx);
    }

    fn poll_mf(&mut self, cx: &mut Context<Self>) {
        self._tasks.push(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(250)).await;
            let (tx, rx) = futures::channel::oneshot::channel();
            rf_win::sta::run(move |_| {
                let _ = tx.send(rf_win::preview_host::media_state());
            });
            let st = rx.await.ok().flatten();
            let gone = this.update(cx, |v, cx| {
                v.mf_state = st;
                // Media Foundation de açamadıysa (süre gelmedi): oynatıcı kapanır, açıklama gösterilir
                if v.failed.is_none() && v.opened_at.elapsed() > v.mf_wait && st.is_none_or(|s| s.duration <= 0.0) {
                    v.failed = Some(t!("Bu biçim desteklenmiyor (ne FilmCraft ne de Windows çözebildi)").to_string());
                    v.mf = false;
                    v.video = false;
                    rf_win::sta::run(|_| rf_win::preview_host::hide());
                }
                cx.notify();
                v.failed.is_some()
            });
            if gone.unwrap_or(true) {
                break;
            }
        }));
    }

    fn arm_hide(&mut self, cx: &mut Context<Self>) {
        self._hide = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HIDE_AFTER).await;
            let _ = this.update(cx, |v, cx| {
                // (hover is not reported while the timeline is dragged: the drag keeps them up)
                if !v.hovered_controls && v.scrub.is_none() && v.video && !v.mf {
                    v.controls = false;
                    cx.notify();
                }
            });
        }));
    }

    fn show_controls(&mut self, cx: &mut Context<Self>) {
        if !self.controls {
            self.controls = true;
            cx.notify();
        }
        self.arm_hide(cx);
    }

    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        if self.rd_mode {
            if let Some(rd) = &self.rd {
                rd.toggle();
            }
            return;
        }
        if self.mf {
            // decided by the player's own thread from its real state (our copy is polled, up to
            // 250 ms old: a second press decided from it repeated the first command); the button
            // follows its event (see `start_mf`)
            rf_win::sta::run(|_| {
                rf_win::preview_host::media_toggle();
            });
            return;
        }
        if let Some(p) = &mut self.player {
            if p.state().playing {
                p.pause();
            } else {
                p.play();
            }
        }
        cx.notify();
    }

    /// The timeline is pressed or dragged to `f` (0..1): the bar goes there at once, the player
    /// follows (at most every `SCRUB_EVERY`, and once more when the drag ends).
    fn scrub_to(&mut self, f: f64, cx: &mut Context<Self>) {
        let f = f.clamp(0.0, 1.0);
        if self.scrub.is_none() {
            self.scrub_sent = None;
            self.scrub_resume = false;
            // our own player would play a scrap of sound after every jump: it waits for the
            // drag to end (its picture follows a jump while paused too)
            if let Some(p) = self.player.as_mut().filter(|p| p.state().playing) {
                p.pause();
                self.scrub_resume = true;
            }
        }
        self.scrub = Some(f);
        if self.scrub_sent.is_none_or(|(_, at)| at.elapsed() >= SCRUB_EVERY) {
            self.scrub_sent = Some((f, Instant::now()));
            self.seek_fraction(f);
        }
        self.show_controls(cx);
        cx.notify();
    }

    /// The button is up: the player goes to where the drag ended and plays on if it was playing.
    fn end_scrub(&mut self, cx: &mut Context<Self>) {
        let Some(f) = self.scrub.take() else { return };
        if self.scrub_sent.take().is_none_or(|(sent, _)| sent != f) {
            self.seek_fraction(f);
        }
        if std::mem::take(&mut self.scrub_resume) {
            if let Some(p) = &mut self.player {
                p.play();
            }
        }
        self.arm_hide(cx);
        cx.notify();
    }

    fn seek_fraction(&mut self, f: f64) {
        let f = f.clamp(0.0, 1.0);
        self.seek_hold = Some((f, Instant::now()));
        if self.rd_mode {
            if let Some(rd) = &self.rd {
                rd.seek_fraction(f);
            }
        } else if self.mf {
            rf_win::sta::run(move |_| rf_win::preview_host::media_seek(f));
        } else if let Some(p) = &mut self.player {
            let d = p.state().duration;
            p.seek(f * d);
        }
    }

    fn set_volume(&mut self, v: f32, cx: &mut Context<Self>) {
        self.volume = v.clamp(0.0, 1.0);
        if let Some(p) = &mut self.player {
            p.set_volume(self.volume);
        }
        if let Some(rd) = &self.rd {
            rd.set_volume(self.volume);
        }
        crate::look::config::update(cx, |s| s.look_volume = self.volume);
        cx.notify();
    }

    fn render_controls(&self, light_text: bool, cx: &mut Context<Self>) -> AnyElement {
        let (playing, pos, dur) = if self.rd_mode {
            self.rd_state.as_ref().map_or((false, 0.0, 0.0), |s| (s.playing, s.position, s.duration))
        } else if self.mf {
            self.mf_state.map_or((false, 0.0, 0.0), |s| (s.playing, s.position, s.duration))
        } else {
            (self.state.playing, self.state.position, self.state.duration)
        };
        let reported = if dur > 0.0 { (pos / dur).clamp(0.0, 1.0) } else { 0.0 };
        // the pointer's place while dragging; after a jump its target, until the player is there
        let held = self.seek_hold.filter(|(f, at)| at.elapsed() < SEEK_HOLD && (reported - f).abs() > 0.01).map(|(f, _)| f);
        let shown = self.scrub.or(held).unwrap_or(reported);
        let pos = if self.scrub.is_some() || held.is_some() { shown * dur } else { pos };
        let frac = shown as f32;
        let scrubbing = self.scrub.is_some();
        // (paused only for the drag: the button keeps saying what happens when it ends)
        let playing = playing || self.scrub_resume;
        let fg: Hsla = if light_text { white().into() } else { rgb(0x1a1a1a).into() };
        let track: Hsla = if light_text { hsla(0., 0., 1., 0.3) } else { hsla(0., 0., 0., 0.2) };
        let accent = cx.global::<gpui_kit::component::Theme>().primary;
        let bar = self.bar.clone();
        let btn = |id: &'static str, icon: L, active: bool| {
            div()
                .id(id)
                .size(px(30.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .when(active, |d| d.bg(hsla(0., 0., 1., 0.18)))
                .hover(|s| s.bg(hsla(0., 0., 1., 0.12)))
                .child(Icon::new(icon).size(px(16.)).text_color(fg))
        };
        h_flex()
            .id("look-media-controls")
            .w_full()
            .gap_2()
            .px_3()
            .py_2()
            .items_center()
            .text_color(fg)
            .text_xs()
            .on_hover(cx.listener(|v, h: &bool, _, _| v.hovered_controls = *h))
            // A press on the controls stops here. The picture behind them toggles play on every
            // press: reaching it, a click on the pause button paused on the press and played
            // again on the release, and the loop and mute buttons paused the video too.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(btn("media-play", if playing { L::Pause } else { L::Play }, false).on_click(cx.listener(|v, _, _, cx| v.toggle_play(cx))))
            .child(div().w(px(84.)).flex_shrink_0().child(format!("{} / {}", clock(pos), clock(dur))))
            .child(
                div()
                    .id("media-seek")
                    .flex_1()
                    .h(px(18.))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .child({
                        let bar = bar.clone();
                        canvas(move |b, _, _| bar.set(Some(b)), |_, _, _, _| {}).absolute().size_full()
                    })
                    .group("media-seek")
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .h(px(4.))
                            .rounded(px(2.))
                            .bg(track)
                            .child(div().h_full().w(relative(frac)).rounded(px(2.)).bg(accent))
                            // the handle: there while the pointer is on the timeline or drags it
                            .child(
                                div()
                                    .absolute()
                                    .top(px(-4.))
                                    .left(relative(frac))
                                    .ml(px(-6.))
                                    .size(px(12.))
                                    .rounded_full()
                                    .bg(fg)
                                    .when(!scrubbing, |d| d.opacity(0.).group_hover("media-seek", |s| s.opacity(1.))),
                            ),
                    )
                    .on_mouse_down(MouseButton::Left, cx.listener(move |v, ev: &MouseDownEvent, _, cx| {
                        if let Some(b) = bar.get() {
                            v.scrub_to((f32::from(ev.position.x - b.origin.x) / f32::from(b.size.width).max(1.0)) as f64, cx);
                        }
                        cx.stop_propagation();
                    }))
                    // dragging: the moves keep coming when the pointer leaves the thin bar (and
                    // the window); the button going up anywhere ends it
                    .on_drag(SeekDrag, |_, _, _, cx| cx.new(|_| SeekGhost))
                    .on_drag_move(cx.listener(|v, ev: &DragMoveEvent<SeekDrag>, _, cx| {
                        if v.scrub.is_none() {
                            return;
                        }
                        if ev.event.pressed_button != Some(MouseButton::Left) {
                            v.end_scrub(cx);
                            return;
                        }
                        let b = ev.bounds;
                        v.scrub_to((f32::from(ev.event.position.x - b.origin.x) / f32::from(b.size.width).max(1.0)) as f64, cx);
                    }))
                    .on_mouse_up(MouseButton::Left, cx.listener(|v, _: &MouseUpEvent, _, cx| v.end_scrub(cx)))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(|v, _: &MouseUpEvent, _, cx| v.end_scrub(cx))),
            )
            .when(!self.mf, |d| {
                d.child(btn("media-loop", L::RotateCw, self.looping).on_click(cx.listener(|v, _, _, cx| {
                    v.looping = !v.looping;
                    if let Some(p) = &mut v.player {
                        p.set_loop(v.looping);
                    }
                    if let Some(rd) = &v.rd {
                        rd.set_looping(v.looping);
                    }
                    let on = v.looping;
                    crate::look::config::update(cx, |s| s.look_loop = on);
                    cx.notify();
                })))
                .child(btn("media-mute", if self.muted { L::Minus } else { L::Music }, self.muted).on_click(cx.listener(|v, _, _, cx| {
                    v.muted = !v.muted;
                    let m = v.muted;
                    if let Some(p) = &mut v.player {
                        p.set_mute(m);
                    }
                    if let Some(rd) = &v.rd {
                        rd.set_muted(m);
                    }
                    cx.notify();
                })))
                .child(div().w(px(34.)).text_right().child(format!("{:.0}%", self.volume * 100.0)))
            })
            .into_any_element()
    }
}

impl Render for MediaView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::theme::Palette::for_window(window);
        if let Some(e) = &self.failed {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .p_4()
                .child(div().text_color(p.text).child(t!("Bu dosya oynatılamıyor")))
                .child(div().text_xs().text_color(p.text_secondary).child(e.clone()))
                .into_any_element();
        }
        if !self.video {
            return self.render_audio(window, cx);
        }
        // video: alana sığan dikdörtgen
        let area = window.viewport_size();
        let (aw, ah) = (f32::from(area.width), f32::from(area.height));
        let (vw, vh) = self.dims;
        let s = (aw / vw).min(ah / vh);
        let (dw, dh) = ((vw * s).floor(), (vh * s).floor());
        let scale = window.scale_factor();
        // converted on the CPU at most at the video's own size: a larger window is filled by the
        // GPU stretching the picture (a 1080p clip full screen on 4K was scaled to 33 MB a frame)
        let k = ((vw / (dw * scale)).min(vh / (dh * scale))).min(1.0);
        self.target = (((dw * scale * k) as u32).max(2), ((dh * scale * k) as u32).max(2));
        let controls = self.controls || self.mf;
        if std::mem::take(&mut self.want_fullscreen) {
            window.toggle_fullscreen();
        }
        let mut root = div()
            .id("look-media")
            .relative()
            .size_full()
            .bg(black())
            .on_mouse_move(cx.listener(|v, ev: &MouseMoveEvent, _, cx| {
                // a drag of the timeline whose button-up never arrived
                if v.scrub.is_some() && ev.pressed_button != Some(MouseButton::Left) {
                    v.end_scrub(cx);
                }
                v.show_controls(cx);
            }))
            .on_scroll(cx.listener(|v, ev: &ScrollEvent, _, cx| {
                let dy = match ev.delta {
                    ScrollDelta::Pixels(p) => f32::from(p.y) / 100.0,
                    ScrollDelta::Lines(l) => l.y / 3.0,
                };
                v.set_volume(v.volume + 0.04 * dy, cx);
                v.show_controls(cx);
            }))
            // A click on the picture plays / pauses at once. A double click is fullscreen: its
            // second press takes the first one's toggle back, so playback is as it was. (The
            // click count keeps growing while clicks come quickly: every even one is a second.)
            .on_mouse_down(MouseButton::Left, cx.listener(|v, ev: &MouseDownEvent, window, cx| {
                v.toggle_play(cx);
                if ev.click_count % 2 == 0 {
                    window.toggle_fullscreen();
                }
            }));
        // the video's own window (Media Foundation, rustdecoder) goes over this rectangle (screen
        // pixels); the controls stay at the bottom
        let rect = [
            ((f32::from(window.bounds().origin.x) + (aw - dw) / 2.0) * scale) as i32,
            ((f32::from(window.bounds().origin.y) + (ah - 48.0 - dh.min(ah - 48.0)) / 2.0) * scale) as i32,
            ((f32::from(window.bounds().origin.x) + (aw + dw) / 2.0) * scale) as i32,
            ((f32::from(window.bounds().origin.y) + (ah - 48.0 + dh.min(ah - 48.0)) / 2.0) * scale) as i32,
        ];
        if self.rd_mode {
            if self.rd_rect != Some(rect) {
                self.rd_rect = Some(rect);
                if let Some(rd) = &self.rd {
                    rd.reposition(rect);
                } else if !self.rd_opening && self.rd_fallback.is_some() {
                    let owner = crate::look::window::hwnd_of(window);
                    self.rd_open(owner, rect, cx);
                }
            }
        } else if self.mf {
            if self.mf_rect != Some(rect) {
                let first = self.mf_rect.is_none();
                self.mf_rect = Some(rect);
                let owner = crate::look::window::hwnd_of(window);
                self.mf_owner = owner;
                let path = self.path.clone();
                let r = windows::Win32::Foundation::RECT { left: rect[0], top: rect[1], right: rect[2], bottom: rect[3] };
                rf_win::sta::run(move |_| {
                    if first {
                        rf_win::preview_host::media_play(owner, &path, Some(r));
                    } else {
                        rf_win::preview_host::media_reposition(r);
                    }
                });
            }
        } else if let Some(img) = self.image.clone() {
            root = root.child(
                gpui_kit::img(img)
                    .absolute()
                    .left(px(((aw - dw) / 2.0).floor()))
                    .top(px(((ah - dh) / 2.0).floor()))
                    .w(px(dw))
                    .h(px(dh))
                    .object_fit(ObjectFit::Fill),
            );
        }
        if controls {
            root = root.child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .bg(linear_gradient(0., linear_color_stop(hsla(0., 0., 0., 0.7), 0.), linear_color_stop(hsla(0., 0., 0., 0.0), 1.)))
                    .pt_6()
                    .child(self.render_controls(true, cx)),
            );
        }
        root.into_any_element()
    }
}

impl MediaView {
    fn render_audio(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let lyric = crate::look::lyrics::line_at(&self.lyrics, self.state.position).map(str::to_string);
        if self.cover.is_none() {
            // (no disk access here: this runs every frame, and the file may be on a network drive)
            if let (Some(images), Some(dir)) = (self.images.clone(), self.path.parent()) {
                let name = super::file_name(&self.path);
                let e = rf_core::Entry::new(&name, rf_core::EntryKind::File, 0, 0, 0, 0);
                let dir = dir.to_path_buf();
                // önbellek kendi dokusunu yönetir: burada bırakılmaz
                let thumb = images.update(cx, |c, cx| c.thumbnail(&dir, &e, 256, cx));
                if let Some(t) = thumb {
                    return self.render_audio_with(t, window, cx, lyric);
                }
            }
        }
        let cover = self.cover.clone();
        self.render_audio_inner(cover, window, cx, lyric)
    }

    fn render_audio_with(&mut self, cover: Arc<RenderImage>, window: &mut Window, cx: &mut Context<Self>, lyric: Option<String>) -> AnyElement {
        self.render_audio_inner(Some(cover), window, cx, lyric)
    }

    fn render_audio_inner(&mut self, cover: Option<Arc<RenderImage>>, window: &mut Window, cx: &mut Context<Self>, lyric: Option<String>) -> AnyElement {
        let p = crate::theme::Palette::for_window(window);
        let field = |k: &str| self.meta.iter().find(|(l, _)| *l == k).map(|(_, v)| v.clone());
        let title = field("Başlık").unwrap_or_else(|| super::file_name(&self.path));
        let artist = field("Sanatçı");
        let album = field("Albüm");
        h_flex()
            .size_full()
            .p_4()
            .gap_4()
            .items_center()
            .child(
                div()
                    .size(px(150.))
                    .flex_shrink_0()
                    .rounded(px(8.))
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(p.card_bg)
                    .map(|d| match cover {
                        Some(c) => d.child(gpui_kit::img(c).size_full().object_fit(ObjectFit::Cover)).into_any_element(),
                        None => d.child(Icon::new(L::Music).size(px(56.)).text_color(p.text_secondary)).into_any_element(),
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).text_color(p.text).truncate().child(title))
                    .when_some(artist, |d, a| d.child(div().text_sm().text_color(p.text_secondary).truncate().child(a)))
                    .when_some(album, |d, a| d.child(div().text_xs().text_color(p.text_tertiary).truncate().child(a)))
                    .when_some(lyric, |d, l| d.child(div().pt_2().text_sm().text_color(p.text).child(l)))
                    .child(div().pt_2().child(self.render_controls(p.dark, cx))),
            )
            .into_any_element()
    }
}
