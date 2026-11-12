//! Shelf pane (Files' "Shelf"): collect items from different folders, then copy or move them all
//! into the folder you are in, or clear the shelf. Items are added from the context menu or with
//! Ctrl+Shift+S; the pane opens on its own when something lands on it.
//!
//! The view only holds the list; the workspace performs the transfer (it knows the active folder).

use std::path::PathBuf;

use gpui_kit::assets::IconName as L;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rf_core::shelf::Shelf;
use rf_win::fileops::Transfer;

use crate::theme::{self, Palette};

pub enum ShelfEvent {
    /// Copy/move every shelf item into the active folder.
    Transfer(Transfer, Vec<PathBuf>),
    /// Show an item in its folder.
    Reveal(PathBuf),
    Close,
}

impl EventEmitter<ShelfEvent> for ShelfView {}

pub struct ShelfView {
    shelf: Shelf,
    _prune: Option<Task<()>>,
}

impl ShelfView {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self { shelf: Shelf::default(), _prune: None }
    }

    pub fn add(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) -> usize {
        let n = self.shelf.add(paths);
        cx.notify();
        n
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.shelf.clear();
        cx.notify();
    }

    /// Drops items that were deleted/moved elsewhere (stats on a background thread).
    pub fn prune(&mut self, cx: &mut Context<Self>) {
        let items = self.shelf.items().to_vec();
        let work = cx.background_spawn(async move { items.into_iter().filter(|p| !p.exists()).collect::<Vec<_>>() });
        self._prune = Some(cx.spawn(async move |this, cx| {
            let gone = work.await;
            if !gone.is_empty() {
                let _ = this.update(cx, |v, cx| {
                    for g in &gone {
                        v.shelf.remove(g);
                    }
                    cx.notify();
                });
            }
        }));
    }

    fn button(id: &'static str, label: &'static str, icon: L, p: &Palette, primary: bool) -> Stateful<Div> {
        h_flex()
            .id(id)
            .gap_1()
            .items_center()
            .px_2()
            .py_1()
            .rounded(theme::RADIUS_SM)
            .text_xs()
            .cursor_pointer()
            .when(primary, |d| d.bg(p.accent).text_color(p.accent_text))
            .when(!primary, |d| d.bg(p.hover).hover(|s| s.bg(p.pressed)))
            .child(Icon::new(icon).size(px(12.)))
            .child(label)
    }
}

impl Render for ShelfView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let items = self.shelf.items().to_vec();
        let empty = items.is_empty();

        let header = h_flex()
            .items_center()
            .justify_between()
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(Icon::new(L::Inbox).size(px(14.)).text_color(p.accent))
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(crate::t!("Raf")))
                    .when(!empty, |d| d.child(div().text_xs().text_color(p.text_tertiary).child(format!("{}", items.len())))),
            )
            .child(
                div()
                    .id("shelf-close")
                    .size(px(22.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::RADIUS_SM)
                    .cursor_pointer()
                    .hover(|s| s.bg(p.hover))
                    .child(Icon::new(L::X).size(px(12.)))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(ShelfEvent::Close))),
            );

        let body: AnyElement = if empty {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px_4()
                .text_xs()
                .text_center()
                .text_color(p.text_tertiary)
                .child(crate::t!("Öğeleri sağ tık → \"Rafa ekle\" (Ctrl+Shift+S) ile buraya toplayın, sonra hepsini istediğiniz klasöre kopyalayın veya taşıyın."))
                .into_any_element()
        } else {
            let mut list = v_flex().id("shelf-list").flex_1().overflow_y_scroll().gap_0p5();
            for (i, path) in items.iter().enumerate() {
                let name = rf_core::history::display_name(path);
                let parent = path.parent().map(rf_core::history::display_name).unwrap_or_default();
                let is_dir_guess = path.extension().is_none();
                let reveal = path.clone();
                let remove = path.clone();
                list = list.child(
                    h_flex()
                        .id(("shelf-item", i))
                        .group("shelf-row")
                        .gap_2()
                        .items_center()
                        .px_2()
                        .py_1()
                        .rounded(theme::RADIUS_SM)
                        .hover(|s| s.bg(p.hover))
                        .cursor_pointer()
                        .on_click(cx.listener(move |_, ev: &ClickEvent, _, cx| {
                            if ev.click_count() >= 2 {
                                cx.emit(ShelfEvent::Reveal(reveal.clone()));
                            }
                        }))
                        .child(
                            Icon::new(if is_dir_guess { L::Folder } else { L::File })
                                .size(px(16.))
                                .text_color(if is_dir_guess { p.accent } else { p.text_secondary }),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_xs().overflow_hidden().text_ellipsis().whitespace_nowrap().child(name))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(p.text_tertiary)
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .child(parent),
                                ),
                        )
                        .child(
                            div()
                                .id(("shelf-rm", i))
                                .invisible()
                                .group_hover("shelf-row", |s| s.visible())
                                .size(px(20.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(theme::RADIUS_SM)
                                .hover(|s| s.bg(p.pressed))
                                .child(Icon::new(L::X).size(px(11.)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.shelf.remove(&remove);
                                    cx.notify();
                                })),
                        ),
                );
            }
            list.into_any_element()
        };

        let actions = h_flex()
            .gap_1()
            .flex_wrap()
            .child(Self::button("shelf-copy", crate::t!("Buraya kopyala"), L::Copy, &p, true).on_click(cx.listener(|this, _, _, cx| {
                let items = this.shelf.items().to_vec();
                if !items.is_empty() {
                    cx.emit(ShelfEvent::Transfer(Transfer::Copy, items));
                }
            })))
            .child(Self::button("shelf-move", crate::t!("Buraya taşı"), L::FolderInput, &p, false).on_click(cx.listener(|this, _, _, cx| {
                let items = this.shelf.items().to_vec();
                if !items.is_empty() {
                    // the workspace clears the shelf once the move has actually started
                    cx.emit(ShelfEvent::Transfer(Transfer::Move, items));
                }
            })))
            .child(Self::button("shelf-clear", crate::t!("Temizle"), L::Trash, &p, false).on_click(cx.listener(|this, _, _, cx| {
                this.shelf.clear();
                cx.notify();
            })));

        v_flex()
            .size_full()
            .gap_2()
            .p_3()
            .child(header)
            .child(body)
            .when(!empty, |d| d.child(actions))
    }
}
