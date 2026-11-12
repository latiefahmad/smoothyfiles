//! Recycle Bin page: list deleted items, restore, empty. All shell work off the UI thread.

use crate::t;
use std::collections::HashSet;

use gpui_kit::assets::IconName as L;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rf_core::format;
use rf_win::recycle::RecycleItem;

use crate::theme::{self, Palette};

pub struct RecycleView {
    items: Vec<RecycleItem>,
    selected: HashSet<usize>,
    loading: bool,
    busy: bool,
    _task: Option<Task<()>>,
}

impl RecycleView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self { items: Vec::new(), selected: HashSet::new(), loading: true, busy: false, _task: None };
        this.reload(cx);
        this
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        let work = cx.background_spawn(async move {
            rf_win::shell::ensure_com();
            rf_win::recycle::list().unwrap_or_default()
        });
        self._task = Some(cx.spawn(async move |this, cx| {
            let mut items = work.await;
            items.sort_by(|a, b| b.deleted_ms.cmp(&a.deleted_ms));
            let _ = this.update(cx, |v, cx| {
                v.items = items;
                v.selected.clear();
                v.loading = false;
                v.busy = false;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn run(&mut self, cx: &mut Context<Self>, f: impl FnOnce() -> anyhow::Result<()> + Send + 'static) {
        self.busy = true;
        let work = cx.background_spawn(async move {
            rf_win::shell::ensure_com();
            if let Err(e) = f() {
                log::warn!("recycle: {e:#}");
            }
        });
        cx.spawn(async move |this, cx| {
            work.await;
            let _ = this.update(cx, |v, cx| v.reload(cx));
        })
        .detach();
        cx.notify();
    }

    fn restore_selected(&mut self, cx: &mut Context<Self>) {
        let items: Vec<RecycleItem> = self.selected.iter().filter_map(|&i| self.items.get(i).cloned()).collect();
        if !items.is_empty() {
            self.run(cx, move || rf_win::recycle::restore(&items));
        }
    }

    fn empty_bin(&mut self, cx: &mut Context<Self>) {
        // confirm = true → Windows asks the user first
        self.run(cx, || rf_win::recycle::empty(true));
    }
}

fn button(id: &'static str, icon: L, label: &'static str, enabled: bool, p: &Palette) -> Stateful<Div> {
    h_flex()
        .id(id)
        .h(px(30.))
        .px_3()
        .gap_2()
        .items_center()
        .rounded(theme::RADIUS_SM)
        .text_sm()
        .bg(p.hover)
        .when(enabled, |d| d.hover(|s| s.bg(p.pressed)).cursor_pointer())
        .when(!enabled, |d| d.opacity(0.5))
        .child(Icon::new(icon).size(px(14.)))
        .child(label)
}

impl Render for RecycleView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let total: u64 = self.items.iter().map(|i| i.size).sum();
        let has_sel = !self.selected.is_empty();
        let header = h_flex()
            .px_4()
            .py_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(p.divider)
            .child(Icon::new(L::Trash).size(px(18.)).text_color(p.text_secondary))
            .child(
                v_flex()
                    .flex_1()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(t!("Geri Dönüşüm Kutusu")))
                    .child(div().text_xs().text_color(p.text_secondary).child(format!(
                        "{} · {}",
                        crate::i18n::items(self.items.len()),
                        format::size(total)
                    ))),
            )
            .child(
                button("rb-restore", L::Undo2, t!("Geri yükle"), has_sel && !self.busy, &p)
                    .on_click(cx.listener(|this, _, _, cx| this.restore_selected(cx))),
            )
            .child(
                button("rb-empty", L::Trash, t!("Kutuyu boşalt"), !self.items.is_empty() && !self.busy, &p)
                    .on_click(cx.listener(|this, _, _, cx| this.empty_bin(cx))),
            );

        let body: AnyElement = if self.loading && self.items.is_empty() {
            div().p_6().text_sm().text_color(p.text_secondary).child(t!("Yükleniyor…")).into_any_element()
        } else if self.items.is_empty() {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Icon::new(L::Trash).size(px(40.)).text_color(p.text_tertiary))
                .child(div().text_color(p.text_secondary).child(t!("Geri dönüşüm kutusu boş")))
                .into_any_element()
        } else {
            let n = self.items.len();
            uniform_list(
                "recycle-rows",
                n,
                cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                    let p = Palette::for_window(window);
                    range
                        .map(|i| {
                            let it = &this.items[i];
                            let sel = this.selected.contains(&i);
                            h_flex()
                                .id(("rb", i))
                                .w_full()
                                .h(theme::ROW_H)
                                .px_3()
                                .gap_2()
                                .items_center()
                                .rounded(theme::RADIUS_SM)
                                .text_sm()
                                .when(sel, |d| d.bg(p.selection))
                                .when(!sel, |d| d.hover(|s| s.bg(p.hover)))
                                .child(
                                    Icon::new(if it.is_dir { L::Folder } else { L::File })
                                        .size(px(16.))
                                        .text_color(p.text_secondary),
                                )
                                .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(it.name.clone()))
                                .child(
                                    div()
                                        .w(px(260.))
                                        .flex_shrink_0()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .text_color(p.text_secondary)
                                        .child(it.original_location.clone()),
                                )
                                .child(div().w(px(130.)).flex_shrink_0().text_color(p.text_secondary).child(format::datetime(it.deleted_ms)))
                                .child(
                                    div()
                                        .w(px(90.))
                                        .flex_shrink_0()
                                        .flex()
                                        .justify_end()
                                        .text_color(p.text_secondary)
                                        .child(format::size(it.size)),
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                        if ev.modifiers.control {
                                            if !this.selected.remove(&i) {
                                                this.selected.insert(i);
                                            }
                                        } else {
                                            this.selected.clear();
                                            this.selected.insert(i);
                                        }
                                        cx.notify();
                                    }),
                                )
                                .into_any_element()
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .flex_1()
            .p_1p5()
            .into_any_element()
        };
        v_flex().size_full().child(header).child(body)
    }
}
