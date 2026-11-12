//! PDF: Windows' own PDF engine (`rf_win::look::pdf`) on a worker thread. Pages are laid out at
//! their real proportions right away and rendered as they scroll into view, at the window's
//! width; only the pages near the view are kept in memory.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use gpui_kit::assets::IconName as L;
use gpui_kit::component::{Icon, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rf_win::look::pdf::{Pdf, PdfError};
use rf_win::thumbs::Bgra;

use crate::look::viewer::{Prepared, Probe, ThemeChoice, Viewer};
use crate::t;

/// Pages kept rendered around the one in view.
const KEEP: usize = 6;
/// Page widths are rendered in steps of this many pixels (no re-render on every resize pixel).
const WIDTH_STEP: f32 = 128.0;
const GAP: f32 = 12.0;
const PAD: f32 = 16.0;

enum Out {
    Page { page: u32, width: u32, img: Bgra },
    /// The real sizes of pages `from..` (until then they are assumed to be the first page's).
    Sizes { from: usize, sizes: Vec<(f32, f32)> },
}

/// Pages whose size is read between two renders (a long PDF opens without reading them all).
const SIZES_AT_ONCE: usize = 64;

/// The picture the GPU draws, taking the rendered page over (no copy of its buffer).
fn render_image(b: Bgra) -> Option<Arc<RenderImage>> {
    let buf = image::RgbaImage::from_raw(b.width, b.height, b.data)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buf)])))
}

#[derive(Default)]
pub struct PdfViewer {
    view: Option<Entity<PdfView>>,
    /// The file opened off the UI thread (a network drive), see `Viewer::loader`.
    opened: Option<Opened>,
}

/// A PDF opened by its worker: the page sizes (or why it can't be shown) and the worker's channels.
struct Opened {
    result: Result<Vec<(f32, f32)>, String>,
    req: mpsc::Sender<(u32, u32)>,
    out: futures::channel::mpsc::UnboundedReceiver<Out>,
}

/// Starts the worker for `path` and waits up to `wait` for it to open the file and report the
/// page count and the first page's size (the window takes its shape; the other pages' sizes
/// follow while it is open), not for any rendering.
fn open_pdf(path: PathBuf, wait: Duration) -> Opened {
    let (opened_tx, opened_rx) = mpsc::channel::<Result<Vec<(f32, f32)>, String>>();
    let (req, req_rx) = mpsc::channel::<(u32, u32)>();
    let (out_tx, out) = futures::channel::mpsc::unbounded::<Out>();
    std::thread::Builder::new()
        .name("directlook-pdf".into())
        .spawn(move || worker(path, opened_tx, req_rx, out_tx))
        .ok();
    let result = opened_rx.recv_timeout(wait).unwrap_or_else(|_| Err(t!("PDF açılamadı.").to_string()));
    Opened { result, req, out }
}

impl Viewer for PdfViewer {
    fn name(&self) -> &'static str {
        "pdf"
    }
    fn priority(&self) -> i32 {
        -1
    }
    fn can_handle(&self, f: &Probe) -> bool {
        f.is_file() && (f.ext == "pdf" || f.head.starts_with(b"%PDF-"))
    }
    fn loader(&self) -> Option<fn(&Probe) -> crate::look::viewer::Loaded> {
        // off the UI thread a slow share may take its time
        Some(|f| Box::new(open_pdf(f.path.clone(), Duration::from_secs(120))))
    }
    fn take_loaded(&mut self, loaded: crate::look::viewer::Loaded) {
        self.opened = loaded.downcast::<Opened>().ok().map(|o| *o);
    }
    fn prepare(&mut self, f: &Probe, cx: &mut App) -> Prepared {
        let name = super::file_name(&f.path);
        let Opened { result, req: req_tx, out: out_rx } = self.opened.take().unwrap_or_else(|| open_pdf(f.path.clone(), Duration::from_secs(5)));
        let (sizes, error) = match result {
            Ok(s) if !s.is_empty() => (s, None),
            Ok(_) => (Vec::new(), Some(t!("Bu PDF'te sayfa yok.").to_string())),
            Err(e) => (Vec::new(), Some(e)),
        };
        let pages = sizes.len();
        let first = sizes.first().copied().unwrap_or((794.0, 1123.0)); // A4
        self.view = Some(cx.new(|cx| PdfView::new(sizes, error, req_tx, out_rx, cx)));
        let title = match pages {
            0 => name,
            1 => format!("{}: {name}", t!("1 sayfa")),
            n => format!("{n} {}: {name}", t!("sayfa")),
        };
        // the window takes the first page's shape, a bit wider for the margins
        let mut p = Prepared::new((first.0 + 2.0 * PAD, first.1 + 2.0 * PAD), title);
        p.fit_ratio = 0.85;
        p.theme = Some(ThemeChoice::Dark);
        p.drag_by_body = false;
        p
    }
    fn render(&mut self, _window: &mut Window, _cx: &mut App) -> AnyElement {
        self.view.as_ref().map_or_else(|| div().into_any_element(), |v| v.clone().into_any_element())
    }
    fn cleanup(&mut self) {
        // dropping the view closes the request channel: the worker ends
        self.view = None;
    }
}

fn worker(path: PathBuf, opened: mpsc::Sender<Result<Vec<(f32, f32)>, String>>, req: mpsc::Receiver<(u32, u32)>, out: futures::channel::mpsc::UnboundedSender<Out>) {
    let pdf = match Pdf::open(&path, None) {
        Ok(p) => p,
        Err(e) => {
            let msg = match e.downcast_ref::<PdfError>() {
                Some(PdfError::Password) => t!("Bu PDF parola ile korunuyor; DirectLook parolalı PDF'leri henüz açmıyor."),
                _ => t!("Bu dosya PDF olarak okunamadı; bozuk olabilir."),
            };
            let _ = opened.send(Err(msg.to_string()));
            return;
        }
    };
    // the first page's size now (every page assumed that size), the rest between renders
    let count = pdf.page_count() as usize;
    let first = if count > 0 { pdf.page_size(0).unwrap_or((794.0, 1123.0)) } else { (794.0, 1123.0) };
    if opened.send(Ok(vec![first; count])).is_err() {
        return;
    }
    let mut next_size = 1usize;
    loop {
        let first = if next_size < count {
            match req.try_recv() {
                Ok(r) => r,
                Err(mpsc::TryRecvError::Empty) => {
                    let to = (next_size + SIZES_AT_ONCE).min(count);
                    let sizes = (next_size..to).map(|i| pdf.page_size(i as u32).unwrap_or(first)).collect();
                    if out.unbounded_send(Out::Sizes { from: next_size, sizes }).is_err() {
                        return;
                    }
                    next_size = to;
                    continue;
                }
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        } else {
            match req.recv() {
                Ok(r) => r,
                Err(_) => return,
            }
        };
        // a fast scroll queues many pages: the newest requests are the ones in view
        let mut batch = vec![first];
        batch.extend(req.try_iter());
        for (page, width) in batch.into_iter().rev() {
            match pdf.render(page, width) {
                Ok(img) => {
                    if out.unbounded_send(Out::Page { page, width, img }).is_err() {
                        return;
                    }
                }
                Err(e) => log::warn!("pdf page {page}: {e:#}"),
            }
        }
    }
}

pub struct PdfView {
    /// Page sizes in DIPs.
    sizes: Vec<(f32, f32)>,
    error: Option<String>,
    list: ListState,
    images: HashMap<u32, (u32, Arc<RenderImage>)>,
    pending: HashSet<(u32, u32)>,
    req: mpsc::Sender<(u32, u32)>,
    _task: Task<()>,
}

impl PdfView {
    fn new(
        sizes: Vec<(f32, f32)>,
        error: Option<String>,
        req: mpsc::Sender<(u32, u32)>,
        mut out: futures::channel::mpsc::UnboundedReceiver<Out>,
        cx: &mut Context<Self>,
    ) -> Self {
        let list = ListState::new(sizes.len(), ListAlignment::Top, px(800.));
        let task = cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            while let Some(o) = out.next().await {
                let (page, width, img) = match o {
                    Out::Page { page, width, img } => (page, width, img),
                    Out::Sizes { from, sizes } => {
                        if this.update(cx, |v, cx| v.set_sizes(from, sizes, cx)).is_err() {
                            break;
                        }
                        continue;
                    }
                };
                let Some(ri) = render_image(img) else { continue };
                let ok = this.update(cx, |v, cx| {
                    v.pending.remove(&(page, width));
                    if let Some((_, old)) = v.images.insert(page, (width, ri)) {
                        cx.drop_image(old, None);
                    }
                    v.evict(cx);
                    cx.notify();
                });
                if ok.is_err() {
                    break;
                }
            }
        });
        Self { sizes, error, list, images: HashMap::new(), pending: HashSet::new(), req, _task: task }
    }

    /// Real sizes of pages that were assumed to be the first page's size: only pages that differ
    /// are measured again.
    fn set_sizes(&mut self, from: usize, sizes: Vec<(f32, f32)>, cx: &mut Context<Self>) {
        let mut changed = false;
        for (k, s) in sizes.into_iter().enumerate() {
            let i = from + k;
            if let Some(old) = self.sizes.get_mut(i) {
                if *old != s {
                    *old = s;
                    self.list.splice(i..i + 1, 1);
                    changed = true;
                }
            }
        }
        if changed {
            cx.notify();
        }
    }

    /// Drops rendered pages far from the one in view.
    fn evict(&mut self, cx: &mut Context<Self>) {
        let top = self.list.logical_scroll_top().item_ix as i64;
        let far: Vec<u32> = self.images.keys().copied().filter(|&p| (p as i64 - top).abs() > KEEP as i64).collect();
        for p in far {
            if let Some((_, img)) = self.images.remove(&p) {
                cx.drop_image(img, None);
            }
        }
    }

    fn render_page(&mut self, ix: usize, window: &mut Window, _cx: &mut Context<Self>) -> AnyElement {
        let Some(&(pw, ph)) = self.sizes.get(ix) else { return div().into_any_element() };
        let avail = (f32::from(window.bounds().size.width) - 2.0 * PAD).max(100.0);
        // shown at the window's width, never larger than 2x the page's own size
        let w = avail.min(pw * 2.0);
        let h = w * ph / pw.max(1.0);
        let scale = window.scale_factor();
        let want = (((w * scale) / WIDTH_STEP).ceil() * WIDTH_STEP) as u32;
        let page = ix as u32;
        let have = self.images.get(&page).map(|(width, img)| (*width, img.clone()));
        if have.as_ref().is_none_or(|(width, _)| *width != want) && !self.pending.contains(&(page, want)) {
            self.pending.insert((page, want));
            let _ = self.req.send((page, want));
        }
        div()
            .w_full()
            .flex()
            .justify_center()
            .pt(px(if ix == 0 { PAD } else { 0. }))
            .pb(px(if ix + 1 == self.sizes.len() { PAD } else { GAP }))
            .child(
                div()
                    .w(px(w))
                    .h(px(h))
                    .bg(gpui_kit::rgb(0xFFFFFF))
                    .shadow_md()
                    // an older, smaller render stretches until the sharp one arrives
                    .when_some(have, |d, (_, img)| d.child(gpui_kit::img(img).size_full())),
            )
            .into_any_element()
    }
}

impl Render for PdfView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(e) = &self.error {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Icon::new(L::File).size(px(40.)))
                .child(div().max_w(px(420.)).text_center().child(e.clone()))
                .into_any_element();
        }
        div()
            .size_full()
            .bg(gpui_kit::rgb(0x2B2B2B))
            .child(list(self.list.clone(), cx.processor(|this, ix, window, cx| this.render_page(ix, window, cx))).size_full())
            .into_any_element()
    }
}
