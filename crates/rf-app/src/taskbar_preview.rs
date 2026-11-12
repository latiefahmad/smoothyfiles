//! A preview of our own taskbar (`smoothyfiles --bar-preview`): the running apps as buttons,
//! grouped like Explorer's taskbar, live. An ordinary window: it doesn't reserve screen space
//! and doesn't touch Explorer's taskbar (phase 2 of notes/design: görev çubuğu).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rf_win::tasklist::{self, Task};

use crate::theme::{self, Palette};

struct Group {
    key: String,
    exe: Option<PathBuf>,
    windows: Vec<Task>,
}

pub struct BarPreview {
    groups: Vec<Group>,
    icons: HashMap<PathBuf, Option<Arc<RenderImage>>>,
    own: isize,
    front: isize,
    _watch: Option<rf_win::tasklist::TaskWatch>,
}

/// Opens the preview window at the bottom of the screen.
pub fn open(cx: &mut App) {
    let display = cx.primary_display().map(|d| d.bounds());
    let (w, h) = (px(960.), px(56.));
    let bounds = match display {
        Some(d) => Bounds::new(point(d.origin.x + (d.size.width - w) / 2., d.origin.y + d.size.height - h - px(64.)), size(w, h)),
        None => Bounds::new(point(px(100.), px(100.)), size(w, h)),
    };
    let opts = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_background: WindowBackgroundAppearance::MicaBackdrop,
        ..gpui_kit::component::TitleBar::window_options()
    };
    let r = gpui_kit::open_window(opts, cx, |window, cx| {
        window.set_window_title("smoothyfiles bar (preview)");
        let own = crate::look::window::hwnd_of(window);
        cx.new(|cx| {
            let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
            let watch = rf_win::tasklist::watch(move |_| {
                let _ = tx.unbounded_send(());
            })
            .map_err(|e| log::warn!("bar watch: {e:#}"))
            .ok();
            cx.spawn(async move |this, cx| {
                use futures::StreamExt;
                while rx.next().await.is_some() {
                    if this.update(cx, |b: &mut BarPreview, cx| b.refresh(cx)).is_err() {
                        break;
                    }
                }
            })
            .detach();
            let mut b = BarPreview { groups: Vec::new(), icons: HashMap::new(), own, front: 0, _watch: watch };
            b.refresh(cx);
            b
        })
    });
    if let Err(e) = r {
        log::error!("bar preview: {e:#}");
    }
}

impl BarPreview {
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let own = self.own;
        let tasks: Vec<Task> = tasklist::tasks().into_iter().filter(|t| t.hwnd != own).collect();
        self.front = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow().0 as isize };
        // keep the buttons in the order they first appeared (a taskbar doesn't reshuffle)
        let mut groups: Vec<Group> = Vec::new();
        for g in &self.groups {
            groups.push(Group { key: g.key.clone(), exe: g.exe.clone(), windows: Vec::new() });
        }
        for t in tasks {
            let k = t.group();
            match groups.iter_mut().find(|g| g.key == k) {
                Some(g) => g.windows.push(t),
                None => groups.push(Group { key: k, exe: t.exe.clone(), windows: vec![t] }),
            }
        }
        groups.retain(|g| !g.windows.is_empty());
        for g in &groups {
            if let Some(exe) = g.exe.clone() {
                if !self.icons.contains_key(&exe) {
                    self.icons.insert(exe.clone(), None);
                    let task = cx.background_executor().spawn({
                        let exe = exe.clone();
                        async move { rf_win::thumbs::get_image(&exe, 32, rf_win::thumbs::Kind::Icon).ok() }
                    });
                    cx.spawn(async move |this, cx| {
                        let Some(bgra) = task.await else { return };
                        let _ = this.update(cx, |b, cx| {
                            if let Some(buf) = image::RgbaImage::from_raw(bgra.width, bgra.height, bgra.data) {
                                b.icons.insert(exe, Some(Arc::new(RenderImage::new(vec![image::Frame::new(buf)]))));
                                cx.notify();
                            }
                        });
                    })
                    .detach();
                }
            }
        }
        self.groups = groups;
        cx.notify();
    }

    /// Front → minimize; otherwise bring the group's latest window forward.
    fn click(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(g) = self.groups.get(ix) else { return };
        let Some(w) = g.windows.first() else { return };
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{SW_MINIMIZE, SW_RESTORE, ShowWindow};
        let h = HWND(w.hwnd as *mut _);
        unsafe {
            if g.windows.iter().any(|w| w.hwnd == self.front) {
                let _ = ShowWindow(h, SW_MINIMIZE);
            } else {
                if w.minimized {
                    let _ = ShowWindow(h, SW_RESTORE);
                }
                rf_win::window::bring_to_front(w.hwnd);
            }
        }
        self.refresh(cx);
    }
}

impl Render for BarPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let mut row = h_flex().size_full().px_2().gap_1().items_center();
        for (ix, g) in self.groups.iter().enumerate() {
            let active = g.windows.iter().any(|w| w.hwnd == self.front);
            let title: SharedString = g.windows.first().map(|w| w.title.clone()).unwrap_or_default().into();
            let tip = g.windows.iter().map(|w| w.title.clone()).collect::<Vec<_>>().join("\n");
            let icon: AnyElement = match g.exe.as_ref().and_then(|e| self.icons.get(e)).cloned().flatten() {
                Some(img) => gpui_kit::img(img).size(px(24.)).into_any_element(),
                None => div()
                    .size(px(24.))
                    .rounded(px(6.))
                    .bg(p.hover)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .child(title.chars().next().map(String::from).unwrap_or_default())
                    .into_any_element(),
            };
            row = row.child(
                v_flex()
                    .id(("task", ix))
                    .w(px(44.))
                    .h(px(44.))
                    .items_center()
                    .justify_center()
                    .gap(px(3.))
                    .rounded(theme::RADIUS_SM)
                    .when(active, |d| d.bg(p.hover))
                    .hover(|s| s.bg(p.hover))
                    .active(|s| s.bg(p.pressed))
                    .tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(w, cx))
                    .child(icon)
                    // running mark: wide and accent when in front, a dot otherwise; a number
                    // for more than one window
                    .child(div().h(px(3.)).rounded_full().w(px(if active { 16. } else { 6. })).bg(if active { p.accent } else { p.text_tertiary }))
                    .on_click(cx.listener(move |b, _, _, cx| b.click(ix, cx))),
            );
            if g.windows.len() > 1 {
                row = row.child(div().text_xs().text_color(p.text_secondary).child(g.windows.len().to_string()));
            }
        }
        div().size_full().text_color(p.text).child(row)
    }
}
