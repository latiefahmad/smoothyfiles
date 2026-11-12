//! "These items already exist", asked when a copy or move meets names already at the
//! destination. Two steps, like Windows 11: first three big answers for everything (replace,
//! skip, keep both), one click each; "Choose for each file…" turns the dialog into a list
//! with a choice per item and both versions' size and date. The copy thread waits for it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::time::SystemTime;

use gpui_kit::assets::IconName as L;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rf_win::fastcopy::{Conflict, Resolve};

use crate::select_box::{Choice, select_box};
use crate::t;
use crate::theme::{self, Palette};

/// Rows drawn in the list; the rest keep both.
const SHOWN: usize = 200;

pub type Reply = Sender<Option<Vec<Resolve>>>;

pub struct ConflictView {
    list: Vec<Conflict>,
    choice: Vec<Resolve>,
    /// The per-item list instead of the three big answers.
    each: bool,
    /// Keyboard highlight among the big answers (0..4).
    hl: usize,
    /// Taken by the first answer (the copy thread waits for exactly one).
    reply: Rc<RefCell<Option<Reply>>>,
    focus: FocusHandle,
}

struct Host(Option<(AnyWindowHandle, Entity<ConflictView>, Option<FocusHandle>)>);
impl Global for Host {}

/// Shows the dialog in `window` (or the active window, or any). No window: both are kept.
pub fn show(list: Vec<Conflict>, reply: Reply, window: Option<AnyWindowHandle>, cx: &mut App) {
    let target = window.or_else(|| cx.active_window()).or_else(|| cx.windows().first().copied());
    let Some(target) = target else {
        let _ = reply.send(Some(vec![Resolve::KeepBoth; list.len()]));
        return;
    };
    // if the window is gone, the reply is dropped: the copy thread takes that as "cancel"
    let r2 = Rc::new(RefCell::new(Some(reply)));
    let _ = target.update(cx, move |_, window, cx| {
        // one at a time: an older question still open is answered "cancel"
        if let Some(Some((_, old, _))) = cx.try_global::<Host>().map(|h| h.0.clone()) {
            old.update(cx, |v, _| v.send(None));
        }
        let restore = window.focused(cx);
        let n = list.len();
        let view = cx.new(|cx| ConflictView { list, choice: vec![Resolve::KeepBoth; n], each: false, hl: 0, reply: r2, focus: cx.focus_handle() });
        window.focus(&view.read(cx).focus.clone(), cx);
        window.activate_window();
        cx.set_global(Host(Some((window.window_handle(), view, restore))));
        window.refresh();
    });
}

pub fn current(window: &Window, cx: &App) -> Option<Entity<ConflictView>> {
    let (handle, view, _) = cx.try_global::<Host>()?.0.as_ref()?;
    (*handle == window.window_handle()).then(|| view.clone())
}

fn close(window: &mut Window, cx: &mut App) {
    let Some(Some((_, _, restore))) = cx.try_global::<Host>().map(|h| h.0.clone()) else { return };
    cx.set_global(Host(None));
    if let Some(f) = restore {
        window.focus(&f, cx);
    }
    window.refresh();
}

fn ms(t: Option<SystemTime>) -> Option<i64> {
    t.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64)
}

/// The big answers, in order: (answer, icon). `None` = "choose for each".
const BIG: [(Option<Resolve>, L); 4] = [
    (Some(Resolve::Replace), L::Replace),
    (Some(Resolve::Skip), L::SkipForward),
    (Some(Resolve::KeepBoth), L::CopyPlus),
    (None, L::ListChecks),
];

impl ConflictView {
    fn send(&mut self, answer: Option<Vec<Resolve>>) {
        if let Some(r) = self.reply.borrow_mut().take() {
            let _ = r.send(answer);
        }
    }

    fn answer(&mut self, answer: Option<Vec<Resolve>>, window: &mut Window, cx: &mut Context<Self>) {
        self.send(answer);
        close(window, cx);
    }

    fn all(&mut self, r: Resolve, window: &mut Window, cx: &mut Context<Self>) {
        let v = vec![r; self.list.len()];
        self.answer(Some(v), window, cx);
    }

    /// Replace where the incoming file is newer, skip the rest.
    fn newer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let v = self
            .list
            .iter()
            .map(|c| match (c.src_time, c.dst_time) {
                (Some(a), Some(b)) if a > b && !c.mismatched() => Resolve::Replace,
                _ => Resolve::Skip,
            })
            .collect();
        self.answer(Some(v), window, cx);
    }

    fn pick_big(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        match BIG[ix].0 {
            Some(r) => self.all(r, window, cx),
            None => {
                self.each = true;
                cx.notify();
            }
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev.keystroke.key.as_str() {
            "escape" if self.each => {
                self.each = false;
                cx.notify();
            }
            "escape" => self.answer(None, window, cx),
            "enter" if self.each => {
                let c = self.choice.clone();
                self.answer(Some(c), window, cx);
            }
            "enter" => self.pick_big(self.hl, window, cx),
            "down" if !self.each => {
                self.hl = (self.hl + 1) % BIG.len();
                cx.notify();
            }
            "up" if !self.each => {
                self.hl = (self.hl + BIG.len() - 1) % BIG.len();
                cx.notify();
            }
            _ => return,
        }
        cx.stop_propagation();
    }
}

impl Drop for ConflictView {
    /// Closed any other way (the window went away): the copy must not wait forever.
    fn drop(&mut self) {
        self.send(None);
    }
}

fn label(r: Resolve) -> &'static str {
    match r {
        Resolve::KeepBoth => t!("İkisini de tut"),
        Resolve::Replace => t!("Değiştir"),
        Resolve::Skip => t!("Atla"),
    }
}

/// "23 bayt · 08.11.2026 05:57" (or "Klasör · …").
fn facts(size: u64, is_dir: bool, time: Option<SystemTime>) -> String {
    let mut parts = vec![if is_dir { t!("Klasör").to_string() } else { rf_core::format::size(size) }];
    if let Some(ms) = ms(time) {
        parts.push(rf_core::format::datetime_full(ms));
    }
    parts.join(" · ")
}

fn side(p: &Palette, title: &str, size: u64, is_dir: bool, time: Option<SystemTime>, newer: bool) -> Div {
    h_flex()
        .gap_1()
        .text_xs()
        .text_color(p.text_secondary)
        .child(div().w(px(56.)).flex_shrink_0().child(title.to_string()))
        .child(div().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(facts(size, is_dir, time)))
        .when(newer, |d| d.child(div().flex_shrink_0().px_1().rounded(px(3.)).bg(p.hover).text_color(p.accent).child(t!("daha yeni"))))
}

fn newer_flags(c: &Conflict) -> (bool, bool) {
    match (c.src_time, c.dst_time) {
        (Some(a), Some(b)) => (a > b, b > a),
        _ => (false, false),
    }
}

impl ConflictView {
    /// The three big answers and "choose for each".
    fn render_big(&self, p: &Palette, cx: &mut Context<Self>) -> Div {
        let one = self.list.len() == 1;
        let text = |r: Option<Resolve>| -> (&'static str, &'static str) {
            match (r, one) {
                (Some(Resolve::Replace), true) => (t!("Dosyayı değiştir"), t!("Gelen dosya, hedeftekinin yerine geçer")),
                (Some(Resolve::Replace), false) => (t!("Dosyaları değiştir"), t!("Gelenler, hedeftekilerin yerine geçer")),
                (Some(Resolve::Skip), true) => (t!("Bu dosyayı atla"), t!("Hedefteki olduğu gibi kalır")),
                (Some(Resolve::Skip), false) => (t!("Bu dosyaları atla"), t!("Hedeftekiler olduğu gibi kalır")),
                (Some(Resolve::KeepBoth), true) => (t!("İkisini de tut"), t!("Gelen dosya \"(2)\" gibi yeni bir adla eklenir")),
                (Some(Resolve::KeepBoth), false) => (t!("İkisini de tut"), t!("Gelenler \"(2)\" gibi yeni adlarla eklenir")),
                (None, _) => (t!("Her dosya için seç…"), t!("Boyutları ve tarihleri karşılaştırarak")),
            }
        };
        let mut col = v_flex().px_4().gap_1();
        for (ix, (r, icon)) in BIG.iter().enumerate() {
            // "choose for each" only makes sense for several
            if r.is_none() && one {
                continue;
            }
            let (title, desc) = text(*r);
            let on = ix == self.hl;
            col = col.child(
                h_flex()
                    .id(("big", ix))
                    .px_3()
                    .py_2p5()
                    .gap_3()
                    .items_center()
                    .rounded(theme::RADIUS_SM)
                    .border_1()
                    .border_color(if on { p.border } else { gpui_kit::transparent_black() })
                    .when(on, |d| d.bg(p.hover))
                    .hover(|s| s.bg(p.hover))
                    .active(|s| s.bg(p.pressed))
                    .cursor_pointer()
                    .child(div().w(px(3.)).h(px(28.)).rounded_full().when(on, |d| d.bg(p.accent)))
                    .child(Icon::new(*icon).size(px(20.)).text_color(if on { p.accent } else { p.text_secondary }))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
                            .child(div().text_xs().text_color(p.text_secondary).child(desc)),
                    )
                    .when(r.is_none(), |d| d.child(Icon::new(L::ChevronRight).size(px(14.)).text_color(p.text_secondary)))
                    .on_hover(cx.listener(move |v, on: &bool, _, cx| {
                        if *on && v.hl != ix {
                            v.hl = ix;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |v, _, window, cx| v.pick_big(ix, window, cx))),
            );
        }
        col
    }

    /// One row per item with its own answer.
    fn render_each(&self, p: &Palette, cx: &mut Context<Self>) -> Stateful<Div> {
        let me = cx.entity();
        let mut rows = v_flex().id("conflict-list").flex_1().min_h_0().overflow_y_scroll().gap_1().px_4();
        for (i, c) in self.list.iter().take(SHOWN).enumerate() {
            let name = c.src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let (src_newer, dst_newer) = newer_flags(c);
            let options: Vec<Resolve> = if c.mismatched() { vec![Resolve::KeepBoth, Resolve::Skip] } else { vec![Resolve::KeepBoth, Resolve::Replace, Resolve::Skip] };
            let current = self.choice[i];
            let me2 = me.clone();
            rows = rows.child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_3()
                    .items_center()
                    .rounded(theme::RADIUS_SM)
                    .bg(p.content_bg)
                    .child(Icon::new(if c.src_dir { L::Folder } else { L::File }).size(px(18.)).text_color(p.text_secondary))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(div().text_sm().overflow_hidden().text_ellipsis().whitespace_nowrap().child(name))
                            .child(side(p, t!("Gelen"), c.src_size, c.src_dir, c.src_time, src_newer))
                            .child(side(p, t!("Hedefte"), c.dst_size, c.dst_dir, c.dst_time, dst_newer)),
                    )
                    .child(div().w(px(170.)).flex_shrink_0().child(select_box(SharedString::from(format!("conflict-{i}")), p, label(current), move |_| {
                        options
                            .iter()
                            .map(|&r| {
                                let me3 = me2.clone();
                                Choice::new(label(r), r == current, move |_, cx| {
                                    me3.update(cx, |v, cx| {
                                        v.choice[i] = r;
                                        cx.notify();
                                    })
                                })
                            })
                            .collect()
                    }))),
            );
        }
        if self.list.len() > SHOWN {
            rows = rows.child(div().px_3().py_2().text_xs().text_color(p.text_secondary).child(format!("+{} · {}", self.list.len() - SHOWN, t!("diğerlerinde ikisi de tutulur"))));
        }
        rows
    }
}

fn button(p: &Palette, id: &'static str, text: &'static str, primary: bool) -> Stateful<Div> {
    div()
        .id(id)
        .min_w(px(96.))
        .h(px(32.))
        .px_4()
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::RADIUS_SM)
        .text_sm()
        .cursor_pointer()
        .bg(if primary { p.accent } else { p.hover })
        .text_color(if primary { p.accent_text } else { p.text })
        .when(!primary, |d| d.border_1().border_color(p.border))
        .hover(|s| s.opacity(0.9))
        .child(text)
}

impl Render for ConflictView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let n = self.list.len();
        let place = |c: Option<&Conflict>, dst: bool| {
            c.and_then(|c| if dst { c.dst.parent() } else { c.src.parent() }).map(rf_core::history::display_name).unwrap_or_default()
        };
        let route = format!("{} → {}", place(self.list.first(), false), place(self.list.first(), true));
        let title = if n == 1 { t!("Bu ad hedefte zaten var").to_string() } else { format!("{} {}", n, t!("öğe hedefte zaten var")) };

        // a single clash: both versions side by side under the title
        let single = (n == 1).then(|| {
            let c = &self.list[0];
            let (src_newer, dst_newer) = newer_flags(c);
            let name = c.src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            h_flex()
                .mx_6()
                .mb_3()
                .px_3()
                .py_2()
                .gap_3()
                .items_center()
                .rounded(theme::RADIUS_SM)
                .bg(p.content_bg)
                .child(Icon::new(if c.src_dir { L::Folder } else { L::File }).size(px(22.)).text_color(p.text_secondary))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(div().text_sm().overflow_hidden().text_ellipsis().whitespace_nowrap().child(name))
                        .child(side(&p, t!("Gelen"), c.src_size, c.src_dir, c.src_time, src_newer))
                        .child(side(&p, t!("Hedefte"), c.dst_size, c.dst_dir, c.dst_time, dst_newer)),
                )
        });

        let body: AnyElement = if self.each { self.render_each(&p, cx).into_any_element() } else { self.render_big(&p, cx).into_any_element() };

        let footer = if self.each {
            h_flex()
                .gap_2()
                .child(
                    button(&p, "cf-back", t!("Geri"), false).on_click(cx.listener(|v, _, _, cx| {
                        v.each = false;
                        cx.notify();
                    })),
                )
                .child(div().flex_1())
                .child(button(&p, "cf-cancel", t!("İptal"), false).on_click(cx.listener(|v, _, window, cx| v.answer(None, window, cx))))
                .child(button(&p, "cf-go", t!("Devam"), true).on_click(cx.listener(|v, _, window, cx| {
                    let c = v.choice.clone();
                    v.answer(Some(c), window, cx);
                })))
        } else {
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .id("cf-newer")
                        .text_sm()
                        .text_color(p.accent)
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .child(t!("Yalnızca daha yenileri değiştir"))
                        .on_click(cx.listener(|v, _, window, cx| v.newer(window, cx))),
                )
                .child(div().flex_1())
                .child(button(&p, "cf-cancel", t!("İptal"), false).on_click(cx.listener(|v, _, window, cx| v.answer(None, window, cx))))
        };

        div()
            .id("conflict-layer")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(hsla(0., 0., 0., 0.35))
            .track_focus(&self.focus)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_key_down(cx.listener(|v, ev: &KeyDownEvent, window, cx| v.on_key(ev, window, cx)))
            .child(
                v_flex()
                    .w(px(if self.each { 640. } else { 480. }))
                    .max_h(relative(0.85))
                    .rounded(theme::RADIUS)
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.border)
                    .shadow_lg()
                    .overflow_hidden()
                    .text_color(p.text)
                    .child(
                        v_flex()
                            .px_6()
                            .pt_6()
                            .pb_4()
                            .gap_1()
                            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(title))
                            .child(div().text_sm().text_color(p.text_secondary).overflow_hidden().text_ellipsis().whitespace_nowrap().child(route)),
                    )
                    .children(single)
                    .child(v_flex().min_h_0().flex_1().child(body))
                    .child(div().px_6().py_4().mt_3().bg(p.hover).border_t_1().border_color(p.divider).child(footer))
                    .with_animation("conflict-in", Animation::new(std::time::Duration::from_millis(160)).with_easing(ease_out_quint()), |el, t| {
                        el.opacity(t).mt(px(10. * (1. - t)))
                    }),
            )
    }
}
