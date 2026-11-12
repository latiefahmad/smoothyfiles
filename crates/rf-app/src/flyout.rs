//! Windows 11 style flyouts with arbitrary content (the "Layout" panel…): a borderless
//! Acrylic popup window under a toolbar button, as tall as its content, closed by clicking
//! elsewhere or Esc. Menus use `fluent_menu`; this is for panels with controls.

use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::v_flex;
use gpui_kit::*;

use crate::theme::Palette;

/// Builds the flyout content on every frame (so it always shows the current state).
pub type Body = Rc<dyn Fn(&Palette, &mut Window, &mut App) -> AnyElement>;

struct Host {
    window: Option<AnyWindowHandle>,
    key: &'static str,
    /// When a flyout closed itself (focus left): a click on its button right after is the
    /// click that closed it, not a request to reopen.
    closed: Option<(&'static str, Instant)>,
}
impl Global for Host {}

fn host(cx: &mut App) -> &mut Host {
    if !cx.has_global::<Host>() {
        cx.set_global(Host { window: None, key: "", closed: None });
    }
    cx.global_mut::<Host>()
}

/// Whether a flyout is open (toolbar buttons hide their tooltips meanwhile).
pub fn is_open(cx: &App) -> bool {
    cx.try_global::<Host>().is_some_and(|h| h.window.is_some())
}

/// Closes the open flyout, if any.
pub fn close(cx: &mut App) {
    if let Some(w) = host(cx).window.take() {
        gpui_windows::KEEP_ACTIVE_LOOK.store(false, std::sync::atomic::Ordering::Relaxed);
        let _ = w.update(cx, |_, window, _| window.remove_window());
    }
}

/// Opens (or, when the same one is open, closes) the flyout `key` under `pos` (owner window
/// coordinates, the flyout's top-left), `width` logical pixels wide.
pub fn toggle(key: &'static str, pos: Point<Pixels>, width: f32, body: Body, owner: &mut Window, cx: &mut App) {
    let h = host(cx);
    if h.window.is_some() && h.key == key {
        close(cx);
        return;
    }
    if h.closed.is_some_and(|(k, t)| k == key && t.elapsed() < Duration::from_millis(350)) {
        h.closed = None;
        return;
    }
    close(cx);
    let vw = f32::from(owner.viewport_size().width);
    let x = f32::from(pos.x).min(vw - width - 8.).max(8.);
    let origin = owner.bounds().origin + point(px(x), pos.y);
    let dark = Palette::for_window(owner).dark;
    let opts = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(origin, size(px(width), px(80.))))),
        titlebar: None,
        focus: true,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Blurred,
        ..Default::default()
    };
    // the app window keeps its active look (Mica, caption) while the flyout has focus
    gpui_windows::KEEP_ACTIVE_LOOK.store(true, std::sync::atomic::Ordering::Relaxed);
    let r = cx.open_window(opts, move |w, cx| {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(h) = w.window_handle() {
            if let RawWindowHandle::Win32(h) = h.as_raw() {
                rf_win::window::set_dark_mode(h.hwnd.get(), dark);
            }
        }
        cx.new(|cx| FlyoutView::new(key, width, body, w, cx))
    });
    match r {
        Ok(w) => {
            let h = host(cx);
            h.window = Some(w.into());
            h.key = key;
        }
        Err(e) => {
            gpui_windows::KEEP_ACTIVE_LOOK.store(false, std::sync::atomic::Ordering::Relaxed);
            log::warn!("flyout: {e:#}")
        }
    }
}

struct FlyoutView {
    key: &'static str,
    width: f32,
    body: Body,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl FlyoutView {
    fn new(key: &'static str, width: f32, body: Body, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subs = vec![cx.observe_window_activation(window, |this: &mut Self, window, cx| {
            if window.is_window_active() {
                return;
            }
            let key = this.key;
            cx.defer(move |cx| {
                let h = host(cx);
                if h.key == key && h.window.is_some() {
                    h.closed = Some((key, Instant::now()));
                    close(cx);
                }
            });
        })];
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self { key, width, body, focus, _subs: subs }
    }
}

impl Render for FlyoutView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let content = (self.body)(&p, window, cx);
        let vh = window.viewport_size().height;
        let width = self.width;
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .text_color(p.text)
            .on_key_down(|ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" {
                    close(cx);
                }
            })
            .child(
                div().relative().w_full().child(content).child(
                    // the window follows the content's height
                    canvas(
                        move |b, window, _| {
                            let h = b.size.height.ceil();
                            if (f32::from(h) - f32::from(vh)).abs() > 1. {
                                window.on_next_frame(move |w, _| w.resize(size(px(width), h)));
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .inset_0(),
                ),
            )
    }
}
