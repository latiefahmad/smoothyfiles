//! Preview / details pane: large thumbnail, text/code preview and file properties for the
//! focused item. Content is read on background threads; nothing here blocks the UI.

use crate::t;
use std::path::{Path, PathBuf};

use gpui_kit::assets::IconName as L;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;
use rf_core::{Entry, format};

use crate::icons::{self, Category};
use crate::image_cache::ImageCache;
use crate::theme::{self, Palette};

const TEXT_LIMIT: usize = 48 * 1024;
const PREVIEW_PX: u32 = 384;

/// Shared folder-size cache (Size column, preview pane, Properties window).
pub fn size_cache() -> &'static rf_win::foldersize::FolderSizeCache {
    rf_win::foldersize::shared_cache()
}

pub struct Preview {
    target: Option<(PathBuf, Entry)>,
    text: Option<(PathBuf, SharedString)>,
    folder_size: Option<(PathBuf, Option<rf_win::foldersize::FolderSize>)>,
    hash: Option<Entity<crate::hash_panel::HashPanel>>,
    /// Media/document properties (duration, resolution, …) for the current target.
    media: Option<(PathBuf, Vec<(&'static str, String)>)>,
    _media_task: Option<Task<()>>,
    size_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    images: Entity<ImageCache>,
    _task: Option<Task<()>>,
    /// Extension → a Windows preview handler exists (looked up off the UI thread).
    handlers: std::collections::HashMap<String, bool>,
    /// What the hosted preview handler currently shows: (file, screen rect).
    host: Option<(PathBuf, [i32; 4])>,
    /// Pane hidden or covered by a dialog/flyout: the host window must not show.
    occluded: bool,
    /// Quick Look (Space): one big visual and a one-line summary instead of the pane layout.
    large: bool,
    /// Quick Look: size of the visual box (measured while painting).
    ql_box: std::cell::Cell<(f32, f32)>,
    /// Archive top level / shortcut target for the current target.
    extra: Option<(PathBuf, Extra)>,
    _extra_task: Option<Task<()>>,
    /// Our own audio / video player for the current target (Media Foundation, in the host
    /// window): created on the first Play, never by just selecting a file.
    player: Option<PlayerUi>,
    /// The last thumbnail shown, and when the target last changed: while the next file's
    /// thumbnail loads, the previous one stays up briefly instead of flashing the type icon.
    last_thumb: Option<std::sync::Arc<RenderImage>>,
    /// An item inside an archive and its extracted copy (thumbnails come from the copy).
    archive_copy: Option<(PathBuf, PathBuf)>,
    _archive_task: Option<Task<()>>,
    changed_at: std::time::Instant,
    _hold_task: Option<Task<()>>,
}

/// How long the previous thumbnail may stand in for one that is still loading.
const HOLD: std::time::Duration = std::time::Duration::from_millis(300);

struct PlayerUi {
    path: PathBuf,
    /// Play was pressed at least once: a video now shows in the host window.
    started: bool,
    /// The video's screen rect last sent to the host.
    rect: Option<[i32; 4]>,
    state: Option<rf_win::preview_host::MediaState>,
    /// The seek bar's bounds (window coordinates), for click-to-seek.
    bar: std::rc::Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
    _poll: Option<Task<()>>,
}

/// A closing window (or tab) must not leave audio playing: the app can stay in the tray.
impl Drop for Preview {
    fn drop(&mut self) {
        if self.player.as_ref().is_some_and(|p| p.started) {
            rf_win::sta::run(|_| rf_win::preview_host::hide());
        }
    }
}

enum Extra {
    /// (name, is folder, size) of the top-level entries, and how many there are in total.
    Archive(Vec<(String, bool, u64)>, usize),
    Shortcut { target: String, args: String, dir: String },
}

fn is_markdown(e: &Entry) -> bool {
    matches!(&*e.ext, "md" | "markdown" | "mdx")
}

fn is_text_like(e: &Entry) -> bool {
    matches!(icons::category(e), Category::Code | Category::Text)
        || matches!(&*e.ext, "csv" | "gitignore" | "env" | "editorconfig" | "gitattributes" | "srt")
}

impl Preview {
    pub fn new(images: Entity<ImageCache>, cx: &mut Context<Self>) -> Self {
        cx.observe(&images, |_, _, cx| cx.notify()).detach();
        Self {
            target: None,
            text: None,
            folder_size: None,
            hash: None,
            media: None,
            _media_task: None,
            size_cancel: Default::default(),
            images,
            _task: None,
            handlers: Default::default(),
            host: None,
            occluded: false,
            large: false,
            ql_box: Default::default(),
            extra: None,
            _extra_task: None,
            player: None,
            last_thumb: None,
            archive_copy: None,
            _archive_task: None,
            changed_at: std::time::Instant::now(),
            _hold_task: None,
        }
    }

    /// The thumbnail to draw for `e`: its own once loaded; meanwhile, for a short while, the
    /// previous one (arrow-keying through photos shouldn't flash an icon between them).
    fn thumb_or_hold(&mut self, dir: &Path, e: &Entry, size: u32, cx: &mut Context<Self>) -> Option<std::sync::Arc<RenderImage>> {
        // an item inside an archive: its extracted copy, once there
        let copy_dir = self
            .archive_copy
            .as_ref()
            .filter(|(p, _)| p.parent() == Some(dir) && p.file_name().is_some_and(|n| n.to_string_lossy() == *e.name))
            .and_then(|(_, real)| real.parent().map(Path::to_path_buf));
        let dir = copy_dir.as_deref().unwrap_or(dir);
        let wants = icons::has_thumbnail(e);
        let thumb = if wants { self.images.update(cx, |c, cx| c.thumbnail(dir, e, size, cx)) } else { None };
        match thumb {
            Some(im) => {
                self.last_thumb = Some(im.clone());
                Some(im)
            }
            None if wants && self.changed_at.elapsed() < HOLD => self.last_thumb.clone(),
            None => None,
        }
    }

    fn render_large(&mut self, path: PathBuf, e: Entry, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = Palette::for_window(window);
        let cat = icons::category(&e);
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let handler = !self.occluded && self.wants_handler(&path, &e, cx);
        let text = self.text.as_ref().filter(|(tp, _)| *tp == path).map(|(_, t)| t.clone());
        // audio / video: our player, as in the pane
        let playable = !handler && self.playable(&path, &e);
        let video = matches!(cat, Category::Video);
        let video_on = playable && video && !self.occluded && self.player.as_ref().is_some_and(|pl| pl.path == path && pl.started);
        let visual: AnyElement = if video_on {
            let me = cx.entity().downgrade();
            let hp = path.clone();
            div()
                .size_full()
                .bg(gpui_kit::black())
                .child(
                    canvas(
                        move |bounds, window, cx| {
                            let _ = me.update(cx, |pv, _| pv.place_video(hp.clone(), bounds, window));
                        },
                        |_, _, _, _| {},
                    )
                    .size_full(),
                )
                .into_any_element()
        } else if handler {
            let me = cx.entity().downgrade();
            let hp = path.clone();
            div()
                .size_full()
                .child(
                    canvas(
                        move |bounds, window, cx| {
                            let _ = me.update(cx, |pv, _| pv.place_host(hp.clone(), bounds, window));
                        },
                        |_, _, _, _| {},
                    )
                    .size_full(),
                )
                .into_any_element()
        } else if let Some(text) = text {
            self.hide_host();
            div()
                .id("ql-text")
                .size_full()
                .overflow_y_scroll()
                .p_4()
                .rounded(theme::RADIUS_SM)
                .bg(p.content_bg)
                .text_sm()
                .when(!is_markdown(&e), |d| d.font_family("Cascadia Mono").child(text.clone()))
                .when(is_markdown(&e), |d| d.child(gpui_kit::component::text::TextView::markdown("ql-md", text.clone())))
                .into_any_element()
        } else {
            self.hide_host();
            let thumb = self.thumb_or_hold(&dir, &e, 1280, cx);
            match thumb {
                Some(im) => {
                    // fit by hand: GPUI sizes images by their aspect ratio from the width, which
                    // overflows a wide box with a tall image
                    let s = im.size(0);
                    let (iw, ih) = ((s.width.0 as f32).max(1.), (s.height.0 as f32).max(1.));
                    let (bw, bh) = self.ql_box.get();
                    let k = (bw / iw).min(bh / ih);
                    let me = cx.entity().downgrade();
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            canvas(
                                move |bounds, _, cx| {
                                    let b = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                                    let _ = me.update(cx, |pv, cx| {
                                        if pv.ql_box.get() != b {
                                            pv.ql_box.set(b);
                                            cx.notify();
                                        }
                                    });
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .size_full(),
                        )
                        .children((bw > 0.).then(|| img(im).w(px(iw * k)).h(px(ih * k))))
                        .into_any_element()
                }
                None => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(icons::icon_for(cat)).size(px(160.)).text_color(theme::kind_color(cat, p.dark)))
                    .into_any_element(),
            }
        };
        // one line: type · size · the first media facts (resolution, duration…)
        let mut facts = vec![icons::type_label(&e).to_string()];
        if !e.is_dir() {
            facts.push(format::size(e.size));
        }
        if let Some((mp, list)) = &self.media {
            if *mp == path {
                facts.extend(list.iter().take(2).map(|(_, v)| v.clone()));
            }
        }
        facts.push(format::datetime(e.modified_ms));
        v_flex()
            .size_full()
            .gap_3()
            // absolute child = a definite box, so `contain` fits instead of overflowing
            .child(div().flex_1().min_h_0().w_full().relative().overflow_hidden().child(div().absolute().inset_0().child(visual)))
            .when(playable, |c| c.child(self.render_player_bar(&p, video, cx)))
            .child(
                h_flex()
                    .w_full()
                    .gap_3()
                    .items_baseline()
                    .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child(SharedString::from(e.name.to_string())))
                    .child(div().text_sm().text_color(p.text_secondary).child(facts.join(" · "))),
            )
            .into_any_element()
    }

    /// The workspace reports each frame whether the pane is shown and uncovered.
    pub fn set_occluded(&mut self, occluded: bool) {
        if occluded && !self.occluded {
            self.hide_host();
            // a video window would float over the menu / dialog: stop (audio may keep playing)
            if self.player.as_ref().is_some_and(|pl| pl.rect.is_some()) {
                self.stop_player();
            }
        }
        self.occluded = occluded;
    }

    /// The window moved: re-send the host position on the next frame.
    pub fn invalidate_host(&mut self) {
        if let Some((_, r)) = &mut self.host {
            *r = [i32::MIN; 4];
        }
    }

    /// The drive is about to be ejected: drop a target on it (a hosted preview handler keeps
    /// the file open, which would block the removal).
    pub fn release_drive(&mut self, letter: char, cx: &mut Context<Self>) {
        if self.target.as_ref().is_some_and(|(p, _)| crate::workspace::on_drive(p, letter)) {
            self.hide_host();
            self.set_target(None, cx);
            cx.notify();
        }
    }

    fn hide_host(&mut self) {
        if self.host.take().is_some() {
            rf_win::sta::run(|_| rf_win::preview_host::hide());
        }
    }

    /// Stops our player (and hides its video).
    fn stop_player(&mut self) {
        if self.player.take().is_some_and(|p| p.started) {
            rf_win::sta::run(|_| rf_win::preview_host::hide());
        }
    }

    /// Audio / video with no Windows preview handler: our own player.
    fn playable(&self, path: &Path, e: &Entry) -> bool {
        !e.is_dir() && !e.is_cloud_placeholder() && matches!(icons::category(e), Category::Video | Category::Audio) && rf_win::preview_host::is_playable(path)
    }

    fn owner(window: &Window) -> Option<isize> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => Some(h.hwnd.get()),
            _ => None,
        }
    }

    fn screen_rect(bounds: Bounds<Pixels>, window: &Window) -> [i32; 4] {
        let scale = window.scale_factor();
        let origin = window.bounds().origin;
        [
            ((f32::from(origin.x) + f32::from(bounds.origin.x)) * scale).round() as i32,
            ((f32::from(origin.y) + f32::from(bounds.origin.y)) * scale).round() as i32,
            ((f32::from(origin.x) + f32::from(bounds.origin.x + bounds.size.width)) * scale).round() as i32,
            ((f32::from(origin.y) + f32::from(bounds.origin.y + bounds.size.height)) * scale).round() as i32,
        ]
    }

    /// Play / pause pressed.
    fn player_toggle(&mut self, video: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some((path, _)) = self.target.clone() else { return };
        let Some(owner) = Self::owner(window) else { return };
        let pl = self.player.get_or_insert_with(|| PlayerUi { path: path.clone(), started: false, rect: None, state: None, bar: Default::default(), _poll: None });
        let playing = pl.state.is_some_and(|s| s.playing);
        if playing {
            rf_win::sta::run(|_| rf_win::preview_host::media_pause());
        } else if !video {
            // audio: no window to show
            pl.started = true;
            rf_win::sta::run(move |_| rf_win::preview_host::media_play(owner, &path, None));
        } else if pl.started {
            let rect = pl.rect.map(|r| windows::Win32::Foundation::RECT { left: r[0], top: r[1], right: r[2], bottom: r[3] });
            rf_win::sta::run(move |_| rf_win::preview_host::media_play(owner, &path, rect));
        } else {
            // the video box takes its place on the next frame and starts playback there
            pl.started = true;
        }
        if pl._poll.is_none() {
            pl._poll = Some(cx.spawn(async move |this, cx| {
                // stops after ~2 s without playback (idle: no wake-ups); Play starts it again
                let mut idle = 0u32;
                loop {
                    cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
                    let (tx, rx) = futures::channel::oneshot::channel();
                    rf_win::sta::run(move |_| {
                        let _ = tx.send(rf_win::preview_host::media_state());
                    });
                    let st = rx.await.ok().flatten();
                    idle = if st.is_some_and(|s| s.playing) { 0 } else { idle + 1 };
                    let alive = this.update(cx, |pv, cx| {
                        let Some(pl) = pv.player.as_mut() else { return false };
                        if pl.state != st {
                            pl.state = st;
                            cx.notify();
                        }
                        if idle >= 8 {
                            pl._poll = None; // drops this task after the update
                            return false;
                        }
                        true
                    });
                    if !matches!(alive, Ok(true)) {
                        break;
                    }
                }
            }));
        }
        cx.notify();
    }

    /// Keeps the video window over `bounds`; the first call starts playback.
    fn place_video(&mut self, path: PathBuf, bounds: Bounds<Pixels>, window: &Window) {
        if self.occluded {
            return;
        }
        let r = Self::screen_rect(bounds, window);
        let Some(owner) = Self::owner(window) else { return };
        let Some(pl) = self.player.as_mut().filter(|p| p.path == path && p.started) else { return };
        if pl.rect == Some(r) {
            return;
        }
        let first = pl.rect.is_none();
        pl.rect = Some(r);
        let rect = windows::Win32::Foundation::RECT { left: r[0], top: r[1], right: r[2], bottom: r[3] };
        rf_win::sta::run(move |_| {
            if first {
                rf_win::preview_host::media_play(owner, &path, Some(rect));
            } else {
                rf_win::preview_host::media_reposition(rect);
            }
        });
    }

    /// Play / pause, a seek bar and the time, under the visual.
    fn render_player_bar(&self, p: &Palette, video: bool, cx: &mut Context<Self>) -> AnyElement {
        let st = self.player.as_ref().and_then(|pl| pl.state);
        let playing = st.is_some_and(|s| s.playing);
        let (pos, dur) = st.map_or((0.0, 0.0), |s| (s.position, s.duration));
        let frac = if dur > 0.0 { (pos / dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let clock = |t: f64| {
            let t = t.max(0.0) as u64;
            if t >= 3600 { format!("{}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60) } else { format!("{}:{:02}", t / 60, t % 60) }
        };
        // before the first Play: the length from the file's properties ("Süre", 00:01:23)
        let known_len = self
            .media
            .as_ref()
            .filter(|(mp, _)| self.target.as_ref().is_some_and(|(t, _)| t == mp))
            .and_then(|(_, list)| list.iter().find(|(k, _)| *k == "Süre"))
            .and_then(|(_, v)| v.split(':').try_fold(0u64, |acc, part| part.trim().parse::<u64>().ok().map(|n| acc * 60 + n)))
            .map(|secs| secs as f64);
        let bar_cell = self.player.as_ref().map(|pl| pl.bar.clone()).unwrap_or_default();
        let bar_for_click = bar_cell.clone();
        h_flex()
            .w_full()
            .h(px(36.))
            .gap_2()
            .items_center()
            .child(
                div()
                    .id("pl-toggle")
                    .size(px(32.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(p.accent)
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.85))
                    .child(Icon::new(if playing { L::Pause } else { L::Play }).size(px(16.)).text_color(gpui_kit::white()))
                    .on_click(cx.listener(move |this, _, window, cx| this.player_toggle(video, window, cx))),
            )
            .child(
                div()
                    .id("pl-seek")
                    .flex_1()
                    .h(px(16.))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .child(
                        div()
                            .w_full()
                            .h(px(4.))
                            .rounded_full()
                            .bg(p.pressed)
                            .child(div().h_full().rounded_full().bg(p.accent).w(relative(frac))),
                    )
                    .child(
                        canvas(
                            move |bounds, _, _| bar_cell.set(Some(bounds)),
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                            let Some(b) = bar_for_click.get() else { return };
                            let w = f32::from(b.size.width).max(1.0);
                            let f = ((f32::from(e.position.x) - f32::from(b.origin.x)) / w).clamp(0.0, 1.0) as f64;
                            if this.player.as_ref().is_some_and(|pl| pl.started) {
                                rf_win::sta::run(move |_| rf_win::preview_host::media_seek(f));
                                if let Some(st) = this.player.as_mut().and_then(|pl| pl.state.as_mut()) {
                                    st.position = st.duration * f;
                                }
                                cx.notify();
                            }
                        }),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(p.text_secondary)
                    .child(format!("{} / {}", clock(pos), if dur > 0.0 { clock(dur) } else { known_len.map_or_else(|| "–".to_string(), clock) })),
            )
            .into_any_element()
    }

    /// Whether `path` should go to a Windows preview handler (Office, PDF, video, fonts…):
    /// everything we don't preview ourselves, when a handler is registered.
    fn wants_handler(&mut self, path: &Path, e: &Entry, cx: &mut Context<Self>) -> bool {
        if e.is_dir() || e.is_cloud_placeholder() || matches!(icons::category(e), Category::Image) || is_text_like(e) {
            return false;
        }
        let ext = e.ext.to_string();
        if ext.is_empty() {
            return false;
        }
        // Design files show the picture saved inside them (their thumbnail): CorelDRAW's preview
        // handler draws only the page, an empty sheet when the design sits beside it. Adobe's
        // handler for Illustrator / EPS draws the artwork itself, so it stays when installed.
        if rf_win::design::is_design_ext(&ext) && !matches!(ext.as_str(), "ai" | "eps" | "epsf") {
            return false;
        }
        match self.handlers.get(&ext) {
            Some(v) => *v,
            None => {
                self.handlers.insert(ext.clone(), false);
                let p = path.to_path_buf();
                let task = cx.background_spawn(async move { rf_win::preview_host::handler_for(&p).is_some() });
                cx.spawn(async move |this, cx| {
                    let ok = task.await;
                    let _ = this.update(cx, |pv, cx| {
                        pv.handlers.insert(ext, ok);
                        if ok {
                            cx.notify();
                        }
                    });
                })
                .detach();
                false
            }
        }
    }

    /// Shows / moves the hosted preview over `bounds` (window coordinates).
    fn place_host(&mut self, path: PathBuf, bounds: Bounds<Pixels>, window: &Window) {
        if self.occluded {
            return;
        }
        let scale = window.scale_factor();
        let origin = window.bounds().origin;
        let r = [
            ((f32::from(origin.x) + f32::from(bounds.origin.x)) * scale).round() as i32,
            ((f32::from(origin.y) + f32::from(bounds.origin.y)) * scale).round() as i32,
            ((f32::from(origin.x) + f32::from(bounds.origin.x + bounds.size.width)) * scale).round() as i32,
            ((f32::from(origin.y) + f32::from(bounds.origin.y + bounds.size.height)) * scale).round() as i32,
        ];
        if self.host.as_ref().is_some_and(|(p, rr)| *p == path && *rr == r) {
            return;
        }
        let same_file = self.host.as_ref().is_some_and(|(p, _)| *p == path);
        self.host = Some((path.clone(), r));
        let owner = {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
                _ => return,
            }
        };
        let dark = Palette::for_window(window).dark;
        let rect = windows::Win32::Foundation::RECT { left: r[0], top: r[1], right: r[2], bottom: r[3] };
        rf_win::sta::run(move |_| {
            if same_file {
                rf_win::preview_host::reposition(rect);
            } else {
                let colors = if dark {
                    rf_win::preview_host::Colors { background: 0x2B2B2B, text: 0xF0F0F0 }
                } else {
                    rf_win::preview_host::Colors { background: 0xF9F9F9, text: 0x1B1B1B }
                };
                rf_win::preview_host::show(owner, &path, rect, &colors);
            }
        });
    }

    /// Called by the workspace whenever the focused item may have changed.
    pub fn set_target(&mut self, target: Option<(PathBuf, Entry)>, cx: &mut Context<Self>) {
        let same = match (&self.target, &target) {
            (Some((a, ea)), Some((b, eb))) => a == b && ea.modified_ms == eb.modified_ms && ea.size == eb.size,
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.target = target;
        self.changed_at = std::time::Instant::now();
        // redraw once the hold is over: the icon then shows if the thumbnail still isn't there
        self._hold_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HOLD).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        }));
        self.stop_player();
        self._task = None;
        self.media = None;
        self._media_task = None;
        self.extra = None;
        self._extra_task = None;
        if let Some((path, e)) = &self.target {
            let p = path.clone();
            let work: Option<Task<Option<Extra>>> = if e.is_dir() || e.is_cloud_placeholder() {
                None
            } else if crate::archive_view::is_browsable(path) {
                Some(cx.background_spawn(async move {
                    let all = crate::archive_view::list_inside(&p, Path::new("")).ok()?;
                    let total = all.len();
                    let mut v: Vec<(String, bool, u64)> = all.into_iter().map(|e| (e.name.to_string(), e.is_dir(), e.size)).collect();
                    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
                    v.truncate(200);
                    Some(Extra::Archive(v, total))
                }))
            } else if &*e.ext == "lnk" {
                Some(cx.background_spawn(async move {
                    let s = rf_win::shortcut::resolve(&p).ok()?;
                    Some(Extra::Shortcut {
                        target: s.target.to_string_lossy().into_owned(),
                        args: s.args,
                        dir: s.working_dir.to_string_lossy().into_owned(),
                    })
                }))
            } else {
                None
            };
            if let Some(work) = work {
                let p = path.clone();
                self._extra_task = Some(cx.spawn(async move |this, cx| {
                    let r = work.await;
                    let _ = this.update(cx, |v, cx| {
                        if let Some(x) = r {
                            v.extra = Some((p, x));
                            cx.notify();
                        }
                    });
                }));
            }
        }
        if let Some((path, e)) = &self.target {
            let rich = matches!(
                icons::category(e),
                Category::Image | Category::Video | Category::Audio | Category::Pdf | Category::Document | Category::Spreadsheet
            );
            if rich && !e.is_dir() && !e.is_cloud_placeholder() {
                let p = path.clone();
                let p2 = path.clone();
                let work = cx.background_spawn(async move { rf_win::props::media_props(&p2) });
                self._media_task = Some(cx.spawn(async move |this, cx| {
                    let props = work.await;
                    let _ = this.update(cx, |v, cx| {
                        if v.target.as_ref().is_some_and(|(tp, _)| *tp == p) {
                            v.media = Some((p, props));
                            cx.notify();
                        }
                    });
                }));
            }
        }
        // Hash panel follows the selection while open (only for files).
        match (&self.target, self.hash.clone()) {
            (Some((p, e)), Some(h)) if !e.is_dir() => h.update(cx, |hp, cx| hp.set_path(p.clone(), cx)),
            (_, Some(_)) => self.hash = None,
            _ => {}
        }
        self.size_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        self.folder_size = None;
        if let Some((path, e)) = &self.target {
            // a folder inside an archive has no size on disk to walk
            if e.is_dir() && !crate::archive_view::is_browsable_path(path) {
                let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                self.size_cancel = cancel.clone();
                let p = path.clone();
                self.folder_size = Some((p.clone(), None));
                let work = cx.background_spawn(async move {
                    let cache = size_cache();
                    cache.get_or_compute(&p, &|| cancel.load(std::sync::atomic::Ordering::Relaxed), &mut |_| {}).ok()
                });
                let p2 = path.clone();
                cx.spawn(async move |this, cx| {
                    let r = work.await;
                    let _ = this.update(cx, |v, cx| {
                        if let (Some((cur, slot)), Some(r)) = (v.folder_size.as_mut(), r) {
                            if *cur == p2 {
                                *slot = Some(r);
                                cx.notify();
                            }
                        }
                    });
                })
                .detach();
            }
        }
        if let Some((path, e)) = &self.target {
            // by the path alone (no disk access on the UI thread): a parent named like an archive
            let in_archive = path.ancestors().skip(1).any(|a| a.as_os_str().len() > 3 && crate::archive_view::is_browsable(a));
            if in_archive && e.size <= crate::archive_view::PREVIEW_EXTRACT_MAX && !e.is_dir() && icons::has_thumbnail(e) {
                // images (and other thumbnailed files) inside an archive: extract, then the
                // thumbnail is made from the copy
                let p2 = path.clone();
                let work = cx.background_spawn(async move { crate::archive_view::real_copy(&p2) });
                let path2 = path.clone();
                self._archive_task = Some(cx.spawn(async move |this, cx| {
                    if let Some(real) = work.await {
                        let _ = this.update(cx, |v, cx| {
                            v.archive_copy = Some((path2, real));
                            cx.notify();
                        });
                    }
                }));
            }
            if !e.is_dir() && is_text_like(e) && e.size > 0 && !e.is_cloud_placeholder() && (!in_archive || e.size <= crate::archive_view::PREVIEW_EXTRACT_MAX) {
                let path = path.clone();
                let p2 = path.clone();
                let read = cx.background_spawn(async move {
                    // inside an archive: the extracted copy (small members only)
                    let real = if p2.exists() { Some(p2) } else { crate::archive_view::real_copy(&p2) };
                    real.and_then(|r| read_text_head(&r))
                });
                self._task = Some(cx.spawn(async move |this, cx| {
                    let text = read.await;
                    let _ = this.update(cx, |v, cx| {
                        v.text = text.map(|t| (path, t.into()));
                        cx.notify();
                    });
                }));
            }
        }
        cx.notify();
    }
}

fn read_text_head(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; TEXT_LIMIT];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    if buf.iter().take(4096).filter(|&&b| b == 0).count() > 2 {
        return None; // binary
    }
    // UTF-16 LE BOM
    if buf.starts_with(&[0xFF, 0xFE]) {
        let u: Vec<u16> = buf[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return Some(String::from_utf16_lossy(&u));
    }
    // UTF-8 (with or without BOM), else the locale's legacy ANSI page (Turkish: Windows-1254)
    let s = rf_win::encoding::decode_text(buf.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&buf));
    // keep it light: at most 400 lines
    Some(s.lines().take(400).collect::<Vec<_>>().join("\n"))
}

/// Archive contents / shortcut target block of the pane.
fn render_extra(p: &Palette, x: &Extra) -> AnyElement {
    match x {
        Extra::Archive(list, total) => {
            let rows = list.iter().map(|(name, dir, size)| {
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .child(Icon::new(if *dir { L::Folder } else { L::File }).size(px(12.)).text_color(p.text_secondary))
                    .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(name.clone()))
                    .children((!*dir).then(|| div().text_color(p.text_tertiary).child(format::size(*size))))
            });
            v_flex()
                .id("pv-archive")
                .w_full()
                .max_h(px(300.))
                .overflow_y_scroll()
                .p_2()
                .gap_1()
                .rounded(theme::RADIUS_SM)
                .bg(p.tile_bg)
                .border_1()
                .border_color(p.border)
                .child(div().text_xs().text_color(p.text_secondary).child(format!("{} — {}", t!("Arşiv içeriği"), crate::i18n::items(*total))))
                .children(rows)
                .into_any_element()
        }
        Extra::Shortcut { target, args, dir } => v_flex()
            .w_full()
            .gap_1p5()
            .child(prop_row(p, t!("Hedef"), target.replace('\\', "\\\u{200B}")))
            .when(!args.is_empty(), |d| d.child(prop_row(p, t!("Bağımsız değişkenler"), args.clone())))
            .when(!dir.is_empty(), |d| d.child(prop_row(p, t!("Başlangıç yeri"), dir.replace('\\', "\\\u{200B}"))))
            .into_any_element(),
    }
}

fn prop_row(p: &Palette, k: &'static str, v: String) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_2()
        .items_start()
        .text_xs()
        .child(div().w(px(92.)).flex_shrink_0().text_color(p.text_secondary).child(k))
        .child(div().flex_1().min_w_0().overflow_hidden().child(v))
}

impl Render for Preview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let mut col = v_flex().id("preview").size_full().p_3().gap_3().overflow_y_scroll();
        let Some((path, e)) = self.target.clone() else {
            self.hide_host();
            return col
                .items_center()
                .justify_center()
                .child(Icon::new(L::PanelRight).size(px(32.)).text_color(p.text_tertiary))
                .child(div().text_sm().text_color(p.text_secondary).child(t!("Önizleme için bir öğe seçin")))
                .into_any_element();
        };
        if self.large {
            return self.render_large(path, e, window, cx);
        }
        let cat = icons::category(&e);
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let handler = !self.occluded && self.wants_handler(&path, &e, cx);
        let playable = !handler && self.playable(&path, &e);
        let video = matches!(cat, Category::Video);
        let video_on = playable && video && !self.occluded && self.player.as_ref().is_some_and(|pl| pl.path == path && pl.started);
        let visual_box: AnyElement = if video_on {
            // our player draws the video here (its own window, kept over this box)
            let me = cx.entity().downgrade();
            let hp = path.clone();
            div()
                .w_full()
                .h(px(232.))
                .flex_shrink_0()
                .rounded(theme::RADIUS)
                .bg(gpui_kit::black())
                .child(
                    canvas(
                        move |bounds, window, cx| {
                            let _ = me.update(cx, |pv, _| pv.place_video(hp.clone(), bounds, window));
                        },
                        |_, _, _, _| {},
                    )
                    .size_full(),
                )
                .into_any_element()
        } else if handler {
            // a Windows preview handler draws here (in its own window, kept over this box)
            let me = cx.entity().downgrade();
            let hp = path.clone();
            div()
                .w_full()
                .h(px(420.))
                .rounded(theme::RADIUS)
                .bg(p.hover)
                .child(
                    canvas(
                        move |bounds, window, cx| {
                            let _ = me.update(cx, |pv, _| pv.place_host(hp.clone(), bounds, window));
                        },
                        |_, _, _, _| {},
                    )
                    .size_full(),
                )
                .into_any_element()
        } else {
            self.hide_host();
            let thumb = self.thumb_or_hold(&dir, &e, PREVIEW_PX, cx);
            let visual: AnyElement = match thumb {
                Some(im) => img(im).w_full().h(px(220.)).object_fit(ObjectFit::Contain).into_any_element(),
                None => Icon::new(icons::icon_for(cat))
                    .size(px(96.))
                    .text_color(theme::kind_color(cat, p.dark))
                    .into_any_element(),
            };
            div()
                .w_full()
                .h(px(232.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(theme::RADIUS)
                .bg(p.hover)
                .child(visual)
                .into_any_element()
        };
        col = col
            .child(visual_box)
            .when(playable, |c| c.child(self.render_player_bar(&p, video, cx)))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(e.name.to_string())),
            );

        if let Some((tp, text)) = &self.text {
            if *tp == path {
                let md = is_markdown(&e);
                col = col.child(
                    div()
                        .id("preview-text")
                        .w_full()
                        .max_h(px(360.))
                        .overflow_y_scroll()
                        .p_2()
                        .rounded(theme::RADIUS_SM)
                        .bg(p.tile_bg)
                        .border_1()
                        .border_color(p.border)
                        .text_xs()
                        .when(!md, |d| d.font_family("Cascadia Mono").text_color(p.text_secondary).child(text.clone()))
                        .when(md, |d| d.child(gpui_kit::component::text::TextView::markdown("pv-md", text.clone()))),
                );
            }
        }
        if let Some((xp, x)) = &self.extra {
            if *xp == path {
                col = col.child(render_extra(&p, x));
            }
        }

        let mut props = v_flex().w_full().gap_1p5();
        props = props.child(prop_row(&p, t!("Tür"), icons::type_label(&e)));
        if e.is_dir() && !crate::archive_view::is_browsable_path(&path) {
            let on_disk = cx.global::<crate::settings::Settings>().folder_size_on_disk;
            let txt = match &self.folder_size {
                Some((fp, Some(fs))) if *fp == path => {
                    let shown = if on_disk { fs.allocated.unwrap_or(fs.bytes) } else { fs.bytes };
                    format!("{} · {} {}, {} {}", format::size(shown), group(fs.files), t!("dosya"), group(fs.dirs), t!("klasör"))
                }
                _ => t!("Hesaplanıyor…").to_string(),
            };
            props = props.child(prop_row(&p, t!("Boyut"), txt));
        }
        if !e.is_dir() {
            props = props.child(prop_row(&p, t!("Boyut"), format!("{} ({} {})", format::size(e.size), group(e.size), t!("bayt"))));
        }
        if let Some((mp, list)) = &self.media {
            if *mp == path {
                for (label, value) in list {
                    props = props.child(prop_row(&p, crate::i18n::t(label), value.clone()));
                }
            }
        }
        props = props
            .child(prop_row(&p, t!("Değiştirilme"), format::datetime(e.modified_ms)))
            .child(prop_row(&p, t!("Oluşturulma"), format::datetime(e.created_ms)))
            // zero-width space after each separator: long paths wrap at folder boundaries
            .child(prop_row(&p, t!("Konum"), dir.to_string_lossy().replace('\\', "\\\u{200B}")));
        let mut flags = Vec::new();
        if e.is_hidden() {
            flags.push(t!("Gizli"));
        }
        if e.is_system() {
            flags.push(t!("Sistem"));
        }
        if e.attrs & rf_core::attrs::READONLY != 0 {
            flags.push(t!("Salt okunur"));
        }
        if e.is_cloud_placeholder() {
            flags.push(t!("Yalnızca çevrimiçi"));
        }
        if e.is_symlink() {
            flags.push(t!("Bağlantı"));
        }
        if !flags.is_empty() {
            props = props.child(prop_row(&p, t!("Öznitelikler"), flags.join(", ")));
        }
        let hash_section: Option<AnyElement> = if e.is_dir() {
            None
        } else if let Some(h) = self.hash.clone() {
            Some(div().w_full().child(h).into_any_element())
        } else {
            let path2 = path.clone();
            Some(
                div()
                    .id("hash-btn")
                    .px_3()
                    .py_1p5()
                    .rounded(theme::RADIUS_SM)
                    .bg(p.hover)
                    .hover(|s| s.bg(p.pressed))
                    .cursor_pointer()
                    .text_sm()
                    .child(t!("Hash hesapla (MD5, SHA…)"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let pth = path2.clone();
                        this.hash = Some(cx.new(|cx| crate::hash_panel::HashPanel::new(pth, cx)));
                        cx.notify();
                    }))
                    .into_any_element(),
            )
        };
        col.child(div().w_full().h(px(1.)).bg(p.divider)).child(props).children(hash_section).into_any_element()
    }
}

/// 1234567 → "1.234.567"
fn group(n: u64) -> String {
    let sep = if crate::i18n::is_tr() { '.' } else { ',' };
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(sep);
        }
        out.push(c);
    }
    out
}
