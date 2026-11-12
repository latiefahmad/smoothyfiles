//! Hızlı Bakış (QuickLook): Boşluk ile açılan önizleme penceresi ve pencere yöneticisi.

pub mod app;
pub mod config;
pub mod geometry;
pub mod ipc;
pub mod trace;
pub mod lyrics;
pub mod system;
pub mod viewer;
pub mod viewers;
pub mod window;

use std::path::PathBuf;

use gpui_kit::component::TitleBar;
use gpui_kit::*;

use crate::look::config::LookSettings;
use crate::settings::LookBackdrop;
use geometry::Rect;
use window::LookWindow;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShowOpts {
    pub topmost: bool,
    pub pin: bool,
}

/// Önizleme pencereleri: sabitlenmemiş tek bir etkin pencere ve sabitlenmişler.
#[derive(Default)]
pub struct LookManager {
    active: Option<WindowHandle<LookWindow>>,
    pinned: Vec<WindowHandle<LookWindow>>,
    /// Küçük resim/simge önbelleği (önizleme pencereleri paylaşır).
    images: Option<Entity<crate::image_cache::ImageCache>>,
}

impl Global for LookManager {}


fn rect(r: [f32; 4]) -> Rect {
    Rect { x: r[0], y: r[1], w: r[2], h: r[3] }
}

fn backdrop(s: &LookSettings) -> WindowBackgroundAppearance {
    match s.look_backdrop {
        LookBackdrop::Mica => WindowBackgroundAppearance::MicaBackdrop,
        LookBackdrop::Acrylic => WindowBackgroundAppearance::Blurred,
        LookBackdrop::Tabbed => WindowBackgroundAppearance::MicaAltBackdrop,
        LookBackdrop::None => WindowBackgroundAppearance::Opaque,
    }
}

impl LookManager {
    fn global(cx: &mut App) -> &mut LookManager {
        cx.default_global::<LookManager>()
    }

    pub fn images(cx: &mut App) -> Entity<crate::image_cache::ImageCache> {
        if let Some(i) = &Self::global(cx).images {
            return i.clone();
        }
        let i = cx.new(crate::image_cache::ImageCache::new);
        Self::global(cx).images = Some(i.clone());
        i
    }

    /// Etkin pencere hâlâ açıksa onu günceller; kapanmışsa unutur.
    fn with_active<R>(cx: &mut App, f: impl FnOnce(&mut LookWindow, &mut Window, &mut Context<LookWindow>) -> R) -> Option<R> {
        let h = Self::global(cx).active?;
        match h.update(cx, f) {
            Ok(r) => Some(r),
            Err(_) => {
                Self::global(cx).active = None;
                Self::report_windows(cx);
                None
            }
        }
    }

    /// Kancaya kendi pencerelerimizi bildirir (öndeyken tuşlar geçerli olsun).
    fn report_windows(cx: &mut App) {
        let m = Self::global(cx);
        let handles: Vec<_> = m.active.iter().chain(m.pinned.iter()).copied().collect();
        let mut hwnds = Vec::new();
        let mut alive = Vec::new();
        for h in handles {
            if let Ok(hw) = h.read_with(cx, |v, _| v.hwnd) {
                hwnds.push(hw);
                alive.push(h);
            }
        }
        let m = Self::global(cx);
        m.pinned.retain(|h| alive.contains(h));
        if m.active.is_some_and(|h| !alive.contains(&h)) {
            m.active = None;
        }
        rf_win::look::hook::set_own_windows(hwnds);
    }

    pub fn show(path: PathBuf, opts: ShowOpts, cx: &mut App) {
        trace::start();
        // kabuk API'leri `C:/a/b` biçimini ayrıştıramaz (özellikler, küçük resimler boş gelir)
        let path = PathBuf::from(path.to_string_lossy().replace('/', "\\"));
        if opts.pin {
            Self::pin_current(cx);
        } else if Self::with_active(cx, |v, w, cx| v.set_path(path.clone(), w, cx)).is_some() {
            if opts.topmost {
                Self::with_active(cx, |v, _, cx| v.set_topmost(true, cx));
            }
            return;
        }
        Self::open(path, opts, cx);
        if opts.pin {
            Self::pin_current(cx);
        }
    }

    fn open(path: PathBuf, opts: ShowOpts, cx: &mut App) {
        let s = cx.global::<LookSettings>().clone();
        let work = rect(rf_win::look::win::work_area_at_cursor());
        // a file in a slow place (a network drive) opens off the UI thread, in the window
        let slow = rf_win::drives::slow_to_read(&path);
        let (viewer, prepared): (Box<dyn viewer::Viewer>, _) =
            if slow { (Box::new(window::Opening), window::Opening::prepared(&path)) } else { window::prepare_viewer(&path, cx) };
        let size = geometry::fit(prepared.preferred, work, prepared.fit_ratio);
        let r = geometry::first_rect(size, work);
        let topmost = opts.topmost || s.look_topmost;
        // imlecin monitörü: GPUI koordinatları o monitörün ölçeğiyle çevirsin (çok monitörlü, farklı DPI)
        let display_id = cx
            .displays()
            .into_iter()
            .find(|d| {
                let b = d.visible_bounds();
                (f32::from(b.origin.x) - work.x).abs() < 2.0 && (f32::from(b.origin.y) - work.y).abs() < 2.0
            })
            .map(|d| d.id());
        let options = WindowOptions {
            display_id,
            window_bounds: Some(WindowBounds::Windowed(Bounds { origin: point(px(r.x), px(r.y)), size: gpui_kit::size(px(r.w), px(r.h)) })),
            window_min_size: Some(gpui_kit::size(px(320.), px(180.))),
            window_background: backdrop(&s),
            focus: s.look_focus_on_open,
            ..TitleBar::window_options()
        };
        let show_in_taskbar = s.look_show_in_taskbar;
        match cx.open_window(options, move |window, cx| {
            let hwnd = window::hwnd_of(window);
            rf_win::look::win::set_show_in_taskbar(hwnd, show_in_taskbar);
            if topmost {
                rf_win::look::win::set_topmost(hwnd, true);
            }
            cx.new(|cx| LookWindow::new(path, topmost, viewer, prepared, window, cx))
        }) {
            Ok(h) => {
                Self::global(cx).active = Some(h);
                Self::report_windows(cx);
                if slow {
                    let _ = h.update(cx, |v, window, cx| v.load_in_background(true, window, cx));
                }
            }
            Err(e) => log::error!("look window: {e:#}"),
        }
    }

    pub fn close(cx: &mut App) {
        Self::with_active(cx, |_, w, _| w.remove_window());
        Self::global(cx).active = None;
        Self::report_windows(cx);
    }

    pub fn reload(cx: &mut App) {
        Self::with_active(cx, |v, w, cx| v.reload(w, cx));
    }

    pub fn fullscreen(cx: &mut App) {
        Self::with_active(cx, |_, w, _| w.toggle_fullscreen());
    }

    /// Seçim değişti: açık pencere yeni dosyayı gösterir.
    pub fn switch(path: PathBuf, cx: &mut App) {
        let path = PathBuf::from(path.to_string_lossy().replace('/', "\\"));
        if Self::current_path(cx).as_ref() == Some(&path) {
            return;
        }
        Self::with_active(cx, |v, w, cx| v.set_path(path, w, cx));
    }

    pub fn run_and_close(cx: &mut App) {
        if let Some(p) = Self::current_path(cx) {
            rf_win::sta::run(move |_| {
                if let Err(e) = rf_win::shell::open(&p) {
                    log::warn!("look run: {e:#}");
                }
            });
        }
        Self::close(cx);
    }

    /// Etkin pencereyi sabitler: açık kalır, bir sonraki önizleme yeni pencere açar.
    pub fn pin_current(cx: &mut App) {
        let Some(h) = Self::global(cx).active.take() else { return };
        if h
            .update(cx, |v, _, cx| {
                v.pinned = true;
                cx.notify();
            })
            .is_ok()
        {
            Self::global(cx).pinned.push(h);
        }
        Self::report_windows(cx);
    }

    pub fn is_visible(cx: &mut App) -> bool {
        Self::with_active(cx, |_, _, _| ()).is_some()
    }

    pub fn current_path(cx: &mut App) -> Option<PathBuf> {
        Self::with_active(cx, |v, _, _| v.path.clone())
    }
}
