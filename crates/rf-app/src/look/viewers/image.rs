//! Resim görüntüleyici: önce kabuk küçük resmi, sonra tam görüntü (arka planda çözülür);
//! tekerlekle %10 yakınlaştırma (sığdır ve %100'de yapışır), sürükle-kaydır, çift tık sığdır,
//! düzey göstergesi, saydamlıkta damalı zemin, animasyon, EXIF katmanı, kopyala, PNG kaydet,
//! renkleri ters çevir, koyu/açık zemin.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rf_win::thumbs::Bgra;

use super::zoom;
use crate::look::viewer::{MoreItem, Prepared, Probe, ThemeChoice, Viewer};
use crate::look::config::LookSettings;
use crate::settings::LookImageTheme;
use crate::t;

/// QuickLook'un resim görüntüleyicisinin tanıdığı uzantılar.
pub const EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jfif", "jpe", "png", "apng", "gif", "bmp", "dib", "ico", "cur", "webp", "avif", "heic", "heif", "tif", "tiff", "svg",
    "svgz", "tga", "psd", "dds", "exr", "hdr", "jxl", "qoi", "pbm", "pgm", "ppm", "pnm", "wdp", "jxr", "arw", "cr2", "cr3", "crw", "dng",
    "erf", "kdc", "mef", "mos", "mrw", "nef", "nrw", "orf", "pef", "raf", "raw", "rw2", "rwl", "sr2", "srf", "srw", "x3f",
];

/// Tam görüntünün en uzun kenarı (GPU doku sınırının altında).
const MAX_SIDE: u32 = 8192;

#[derive(Default)]
pub struct ImageViewer {
    view: Option<Entity<ImageView>>,
}

impl Viewer for ImageViewer {
    fn name(&self) -> &'static str {
        "image"
    }
    fn priority(&self) -> i32 {
        0
    }
    fn can_handle(&self, f: &Probe) -> bool {
        // design files (CorelDRAW, EPS, Krita…): the picture saved inside them
        f.is_file() && (EXTENSIONS.contains(&f.ext.as_str()) || rf_win::design::sniff(&f.ext, &f.head))
    }
    fn prepare(&mut self, f: &Probe, cx: &mut App) -> Prepared {
        let name = super::file_name(&f.path);
        let dims = rf_win::look::decode::dimensions(&f.path);
        let path = f.path.clone();
        self.view = Some(cx.new(|cx| ImageView::new(path, dims, cx)));
        let (w, h) = dims.map_or((800.0, 600.0), |(w, h)| (w as f32, h as f32));
        let title = match dims {
            // (a design file's picture is its preview, not the design's size)
            _ if rf_win::design::is_design_ext(&f.ext) => name,
            Some((w, h)) => format!("{w}×{h}: {name}"),
            None => name,
        };
        let mut p = Prepared::new((w.max(240.0), h.max(160.0)), title);
        p.overlay_titlebar = true;
        p.drag_by_body = false;
        p.theme = Some(match cx.global::<LookSettings>().look_image_theme {
            LookImageTheme::Dark => ThemeChoice::Dark,
            LookImageTheme::Light => ThemeChoice::Light,
        });
        p
    }
    fn render(&mut self, _window: &mut Window, _cx: &mut App) -> AnyElement {
        self.view.as_ref().map_or_else(|| div().into_any_element(), |v| v.clone().into_any_element())
    }
    fn more_menu(&self) -> Vec<MoreItem> {
        let Some(v) = self.view.clone() else { return vec![] };
        let item = |label: &'static str, f: fn(&mut ImageView, &mut Window, &mut Context<ImageView>)| {
            let v = v.clone();
            MoreItem { label: label.into(), action: Rc::new(move |window: &mut Window, cx: &mut App| v.update(cx, |iv, cx| f(iv, window, cx))) }
        };
        vec![
            item(t!("Panoya kopyala"), |v, _, cx| v.copy(cx)),
            item(t!("PNG olarak kaydet…"), |v, _, cx| v.save_png(cx)),
            item(t!("Renkleri ters çevir"), |v, _, cx| v.toggle_invert(cx)),
            item(t!("EXIF bilgisi"), |v, _, cx| {
                v.show_exif = !v.show_exif;
                cx.notify();
            }),
            item(t!("Zemin: koyu/açık"), |_, _, cx| {
                crate::look::config::update(cx, |s| {
                    s.look_image_theme = match s.look_image_theme {
                        LookImageTheme::Dark => LookImageTheme::Light,
                        LookImageTheme::Light => LookImageTheme::Dark,
                    }
                });
                cx.notify();
            }),
        ]
    }
    fn cleanup(&mut self) {
        self.view = None;
    }
}

enum State {
    Loading,
    Ready,
    Failed(String),
}

pub struct ImageView {
    path: PathBuf,
    /// Asıl boyut (döndürülmüş); yakınlaştırma buna göre.
    dims: Option<(f32, f32)>,
    thumb: Option<Arc<RenderImage>>,
    /// Each frame's delay (ms); the pixels live once, in `frames`.
    delays: Vec<u32>,
    frames: Vec<Arc<RenderImage>>,
    frame: usize,
    has_alpha: bool,
    exif: Vec<(String, String)>,
    state: State,
    /// None = sığdır.
    zoom: Option<f32>,
    offset: (f32, f32),
    drag: Option<((f32, f32), (f32, f32))>,
    label_until: Option<Instant>,
    inverted: bool,
    show_exif: bool,
    _tasks: Vec<Task<()>>,
}

fn render_image(b: &Bgra) -> Option<Arc<RenderImage>> {
    render_image_owned(Bgra { width: b.width, height: b.height, data: b.data.clone() })
}

/// [`render_image`] taking the pixels over (no copy: a 24 MP photo is a 96 MB buffer).
fn render_image_owned(b: Bgra) -> Option<Arc<RenderImage>> {
    let buf = image::RgbaImage::from_raw(b.width, b.height, b.data)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buf)])))
}

/// A frame's pixels back out of the picture the GPU draws.
fn pixels_of(img: &RenderImage) -> Option<Bgra> {
    let s = img.size(0);
    Some(Bgra { width: s.width.0 as u32, height: s.height.0 as u32, data: img.as_bytes(0)?.to_vec() })
}

/// The decoded image as the view keeps it, made off the UI thread.
struct Ready {
    width: u32,
    height: u32,
    exif: Vec<(String, String)>,
    has_alpha: bool,
    delays: Vec<u32>,
    frames: Vec<Arc<RenderImage>>,
}

fn ready(d: rf_win::look::decode::Decoded) -> Ready {
    let has_alpha = d.frames.first().is_some_and(|(b, _)| b.data.chunks_exact(4).any(|p| p[3] < 255));
    let delays = d.frames.iter().map(|(_, ms)| *ms).collect();
    let frames = d.frames.into_iter().filter_map(|(b, _)| render_image_owned(b)).collect();
    Ready { width: d.width, height: d.height, exif: d.exif, has_alpha, delays, frames }
}

fn inverted(b: &Bgra) -> Bgra {
    let mut data = b.data.clone();
    for px in data.chunks_exact_mut(4) {
        px[0] = 255 - px[0];
        px[1] = 255 - px[1];
        px[2] = 255 - px[2];
    }
    Bgra { width: b.width, height: b.height, data }
}

/// Damalı zemin dokusu (bir kez üretilir; pencere iş parçacığında).
fn checker(dark: bool) -> Arc<RenderImage> {
    thread_local! {
        static C: std::cell::RefCell<[Option<Arc<RenderImage>>; 2]> = const { std::cell::RefCell::new([None, None]) };
    }
    C.with(|c| {
        let mut c = c.borrow_mut();
        let slot = &mut c[dark as usize];
        slot.get_or_insert_with(|| {
            let (n, cell) = (1024u32, 8u32);
            let (a, b) = if dark { (0x2a, 0x36) } else { (0xff, 0xe6) };
            let mut data = Vec::with_capacity((n * n * 4) as usize);
            for y in 0..n {
                for x in 0..n {
                    let v = if ((x / cell) + (y / cell)) % 2 == 0 { a } else { b };
                    data.extend_from_slice(&[v, v, v, 255]);
                }
            }
            render_image(&Bgra { width: n, height: n, data }).expect("checker")
        })
        .clone()
    })
}

impl ImageView {
    fn new(path: PathBuf, dims: Option<(u32, u32)>, cx: &mut Context<Self>) -> Self {
        let mut v = Self {
            path: path.clone(),
            dims: dims.map(|(w, h)| (w as f32, h as f32)),
            thumb: None,
            delays: Vec::new(),
            frames: Vec::new(),
            frame: 0,
            has_alpha: false,
            exif: Vec::new(),
            state: State::Loading,
            zoom: None,
            offset: (0.0, 0.0),
            drag: None,
            label_until: None,
            inverted: false,
            show_exif: false,
            _tasks: Vec::new(),
        };
        // pencere kapanınca ya da başka dosyaya geçilince GPU dokuları bırakılır
        cx.on_release(|v: &mut Self, cx| {
            for img in v.frames.drain(..).chain(v.thumb.take()) {
                cx.drop_image(img, None);
            }
        })
        .detach();
        // önce kabuk küçük resmi (çoğunlukla önbellekte, anında)
        let p = path.clone();
        let thumb = cx.background_spawn(async move { rf_win::thumbs::get_image(&p, 256, rf_win::thumbs::Kind::Thumbnail).ok() });
        v._tasks.push(cx.spawn(async move |this, cx| {
            if let Some(t) = thumb.await {
                let _ = this.update(cx, |v, cx| {
                    if v.frames.is_empty() {
                        v.thumb = render_image(&t);
                        cx.notify();
                    }
                });
            }
        }));
        let full = cx.background_spawn(async move { rf_win::look::decode::decode_full(&path, MAX_SIDE).map(ready) });
        v._tasks.push(cx.spawn(async move |this, cx| {
            let r = full.await;
            let _ = this.update(cx, |v, cx| v.loaded(r, cx));
        }));
        v
    }

    fn loaded(&mut self, r: anyhow::Result<Ready>, cx: &mut Context<Self>) {
        crate::look::trace::mark("tam resim");
        match r {
            Ok(d) => {
                self.dims = Some((d.width as f32, d.height as f32));
                self.exif = d.exif;
                self.has_alpha = d.has_alpha;
                self.delays = d.delays;
                for img in std::mem::replace(&mut self.frames, d.frames) {
                    cx.drop_image(img, None);
                }
                // (inverted before it arrived: turn it now)
                if self.inverted {
                    self.inverted = false;
                    self.toggle_invert(cx);
                }
                if let Some(t) = self.thumb.take() {
                    cx.drop_image(t, None);
                }
                self.state = State::Ready;
                if self.frames.len() > 1 {
                    self.animate(cx);
                }
            }
            Err(e) => {
                log::warn!("look image {}: {e:#}", self.path.display());
                self.state = State::Failed(e.to_string());
            }
        }
        cx.notify();
    }

    fn animate(&mut self, cx: &mut Context<Self>) {
        let delays: Vec<u32> = self.delays.clone();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let mut i = 0usize;
            loop {
                cx.background_executor().timer(Duration::from_millis(delays[i % delays.len()].max(20) as u64)).await;
                i += 1;
                if this
                    .update(cx, |v, cx| {
                        v.frame = i % v.frames.len().max(1);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn toggle_invert(&mut self, cx: &mut Context<Self>) {
        self.inverted = !self.inverted;
        cx.notify();
        if self.frames.is_empty() {
            return; // turned when the image arrives
        }
        // the frames turned on a helper thread (inverting twice gives the original back)
        let frames = self.frames.clone();
        let want = self.inverted;
        let work = cx.background_spawn(async move { frames.iter().filter_map(|f| pixels_of(f)).filter_map(|b| render_image_owned(inverted(&b))).collect::<Vec<_>>() });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let turned = work.await;
            let _ = this.update(cx, |v, cx| {
                if v.inverted != want {
                    return; // toggled again meanwhile: that one wins
                }
                for img in std::mem::replace(&mut v.frames, turned) {
                    cx.drop_image(img, None);
                }
                cx.notify();
            });
        }));
    }

    fn png_bytes(&self) -> Option<Vec<u8>> {
        let f = self.frames.get(self.frame.min(self.frames.len().saturating_sub(1)))?;
        // (the shown frame: already inverted when that is on)
        let b = pixels_of(f)?;
        let mut rgba = b.data;
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        let img = image::RgbaImage::from_raw(b.width, b.height, rgba)?;
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).ok()?;
        Some(out.into_inner())
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(png) = self.png_bytes() {
            cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(ImageFormat::Png, png)));
        }
    }

    fn save_png(&mut self, cx: &mut Context<Self>) {
        let Some(png) = self.png_bytes() else { return };
        let dir = self.path.parent().map(Path::to_path_buf).unwrap_or_default();
        let stem = self.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "resim".into());
        let rx = cx.prompt_for_new_path(&dir, Some(&format!("{stem}.png")));
        cx.spawn(async move |_, _| {
            if let Ok(Ok(Some(target))) = rx.await {
                if let Err(e) = std::fs::write(&target, png) {
                    log::warn!("look save png: {e}");
                }
            }
        })
        .detach();
    }

    fn show_label(&mut self, cx: &mut Context<Self>) {
        self.label_until = Some(Instant::now() + Duration::from_millis(900));
        self._tasks.push(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(950)).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        }));
        if self._tasks.len() > 16 {
            self._tasks.drain(2..self._tasks.len() - 4);
        }
    }

    /// Şu anki ölçek, sığdırma ölçeği ve gösterilen boyut.
    fn layout(&self, area: (f32, f32)) -> Option<(f32, f32, (f32, f32))> {
        let dims = self.dims?;
        let fit = zoom::fit_zoom(dims, area);
        let z = self.zoom.unwrap_or(fit);
        Some((z, fit, (dims.0 * z, dims.1 * z)))
    }
}

fn area_of(window: &Window) -> (f32, f32) {
    let s = window.viewport_size();
    (f32::from(s.width), f32::from(s.height))
}

impl Render for ImageView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let light = cx.global::<LookSettings>().look_image_theme == LookImageTheme::Light;
        let (bg, fg) = if light { (rgb(0xf3f3f3), rgb(0x1a1a1a)) } else { (rgb(0x1c1c1c), rgb(0xf0f0f0)) };
        let area = area_of(window);
        let layout = self.layout(area);
        if let Some((_, _, shown)) = layout {
            self.offset = zoom::clamp_offset(self.offset, shown, area);
        }
        let (ox, oy) = self.offset;
        let image = self.frames.get(self.frame).cloned().or_else(|| self.thumb.clone());
        let label = self.label_until.filter(|t| Instant::now() < *t).and(layout).map(|(z, ..)| format!("{:.0}%", z * 100.0));

        let mut root = div()
            .id("look-image")
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(bg)
            .on_scroll(cx.listener(|v, ev: &ScrollEvent, window, cx| {
                // çentik: Windows'ta 3 satır (yumuşak kaydırma bunu parçalara bölerek gönderir);
                // dokunmatik yüzeyin piksel kaydırmasında ~100 piksel
                let dy = match ev.delta {
                    ScrollDelta::Pixels(p) => f32::from(p.y) / 100.0,
                    ScrollDelta::Lines(l) => l.y / 3.0,
                };
                if dy == 0.0 {
                    return;
                }
                let area = area_of(window);
                let Some((z, fit, _)) = v.layout(area) else { return };
                let nz = zoom::step(z, fit, dy);
                let cursor = (f32::from(ev.position.x), f32::from(ev.position.y));
                v.offset = zoom::zoom_offset(v.offset, cursor, z, nz);
                v.zoom = if (nz - fit).abs() < 1e-6 { None } else { Some(nz) };
                v.show_label(cx);
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(|v, ev: &MouseDownEvent, _, cx| {
                if ev.click_count == 2 {
                    v.zoom = None;
                    v.show_label(cx);
                    cx.notify();
                    return;
                }
                v.drag = Some(((f32::from(ev.position.x), f32::from(ev.position.y)), v.offset));
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|v, _: &MouseUpEvent, _, _| v.drag = None))
            .on_mouse_move(cx.listener(|v, ev: &MouseMoveEvent, _, cx| {
                if let Some((start, from)) = v.drag {
                    if ev.pressed_button != Some(MouseButton::Left) {
                        v.drag = None;
                        return;
                    }
                    v.offset = (from.0 + f32::from(ev.position.x) - start.0, from.1 + f32::from(ev.position.y) - start.1);
                    cx.notify();
                }
            }));

        if let (Some(img_src), Some((_, _, shown))) = (image, layout) {
            if self.has_alpha && !self.inverted {
                root = root.child(
                    div()
                        .absolute()
                        .left(px(ox))
                        .top(px(oy))
                        .w(px(shown.0))
                        .h(px(shown.1))
                        .overflow_hidden()
                        .child(gpui_kit::img(checker(!light)).w(px(1024.)).h(px(1024.))),
                );
            }
            root = root.child(gpui_kit::img(img_src).absolute().left(px(ox)).top(px(oy)).w(px(shown.0)).h(px(shown.1)).object_fit(ObjectFit::Fill));
        } else if let State::Failed(e) = &self.state {
            root = root.child(
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .text_color(fg)
                    .child(t!("Bu resim açılamadı"))
                    .child(div().text_xs().opacity(0.7).child(e.clone())),
            );
        }
        if let Some(l) = label {
            root = root.child(
                div().absolute().inset_0().flex().items_center().justify_center().child(
                    div().px_4().py_2().rounded(px(8.)).bg(hsla(0., 0., 0., 0.6)).text_color(white()).text_lg().child(l),
                ),
            );
        }
        if self.show_exif && !self.exif.is_empty() {
            root = root.child(
                v_flex()
                    .id("look-exif")
                    .absolute()
                    .left(px(12.))
                    .bottom(px(12.))
                    .max_h(px((area.1 - 80.0).max(100.0)))
                    .overflow_y_scroll()
                    .p_3()
                    .gap_0p5()
                    .rounded(px(8.))
                    .bg(hsla(0., 0., 0., 0.65))
                    .text_color(white())
                    .text_xs()
                    .children(self.exif.iter().map(|(k, v)| h_flex().gap_2().child(div().opacity(0.7).child(k.clone())).child(v.clone()))),
            );
        }
        root.when(matches!(self.state, State::Loading) && self.thumb.is_none(), |d| {
            d.child(div().absolute().inset_0().flex().items_center().justify_center().text_color(fg).opacity(0.6).child(t!("Yükleniyor…")))
        })
    }
}
