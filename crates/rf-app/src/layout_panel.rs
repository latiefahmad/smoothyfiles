//! The "Layout" flyout of the command bar, shaped like Files': five layout tiles, a size
//! slider, and switches for hidden items, file extensions and adaptive layout (plus our dual
//! pane and shelf). Everything applies to the current folder (remembered per folder).

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::assets::IconName as L;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::folder_view::{FolderView, GridStyle, ViewMode};
use crate::t;
use crate::theme::{self, Palette};

const WIDTH: f32 = 436.;
const STOPS: usize = 5;

pub fn open(view: WeakEntity<FolderView>, dual: bool, shelf: bool, pos: Point<Pixels>, window: &mut Window, cx: &mut App) {
    let owner = window.window_handle();
    let dual = Rc::new(Cell::new(dual));
    let shelf = Rc::new(Cell::new(shelf));
    let body: crate::flyout::Body = Rc::new(move |p, window, cx| {
        let Some(v) = view.upgrade() else { return div().into_any_element() };
        render(&v, owner, dual.clone(), shelf.clone(), p, window, cx)
    });
    crate::flyout::toggle("layout", pos, WIDTH, body, window, cx);
}

fn section(p: &Palette, title: &str) -> Div {
    div().px_4().pt_3().pb_2().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.text).child(title.to_string())
}

fn divider(p: &Palette) -> Div {
    div().mt_2().h(px(1.)).w_full().bg(p.divider)
}

fn render(v: &Entity<FolderView>, owner: AnyWindowHandle, dual: Rc<Cell<bool>>, shelf: Rc<Cell<bool>>, p: &Palette, _window: &mut Window, cx: &mut App) -> AnyElement {
    let (mode, style, size, hidden) = {
        let r = v.read(cx);
        (r.view_mode, r.grid_style, r.layout_size(), r.filter.show_hidden)
    };
    let s = cx.global::<crate::settings::Settings>().clone();
    let weak = v.downgrade();

    // ---- layout tiles
    let tile = |id: &'static str, icon: L, label: &str, m: ViewMode, st: GridStyle| {
        let on = mode == m && (m != ViewMode::Grid || style == st);
        let w = weak.clone();
        v_flex()
            .id(id)
            .w(px(76.))
            .h(px(68.))
            .gap_1()
            .items_center()
            .justify_center()
            .rounded(theme::RADIUS)
            .text_xs()
            .when(on, |d| d.bg(p.accent).text_color(p.accent_text))
            .when(!on, |d| d.bg(p.hover).hover(|s| s.bg(p.pressed)))
            .child(Icon::new(icon).size(px(20.)))
            .child(label.to_string())
            .on_click(move |_, window, cx| {
                let _ = w.update(cx, |v, cx| v.set_layout(m, st, cx));
                window.refresh();
            })
    };
    let tiles = h_flex()
        .px_4()
        .gap_2()
        .justify_between()
        .child(tile("lt-details", L::List, t!("Ayrıntılar"), ViewMode::Details, GridStyle::Tiles))
        .child(tile("lt-list", L::TextAlignJustify, t!("Liste"), ViewMode::Grid, GridStyle::List))
        .child(tile("lt-cards", L::LayoutList, t!("Kartlar"), ViewMode::Grid, GridStyle::Cards))
        .child(tile("lt-grid", L::LayoutGrid, t!("Izgara"), ViewMode::Grid, GridStyle::Tiles))
        .child(tile("lt-cols", L::Columns3, t!("Sütunlar"), ViewMode::Columns, GridStyle::Tiles));

    // ---- size slider (Details row height / grid tile size)
    let enabled = size.is_some();
    let cur = size.unwrap_or(1) as usize;
    let frac = cur as f32 / (STOPS - 1) as f32;
    let track_color = if enabled { p.accent } else { p.text_tertiary };
    let mut stops = h_flex().absolute().inset_0();
    for i in 0..STOPS {
        let w = weak.clone();
        stops = stops.child(
            div()
                .id(("lt-stop", i))
                .flex_1()
                .h_full()
                .when(enabled, |d| d.cursor_pointer())
                .on_click(move |_, window, cx| {
                    let _ = w.update(cx, |v, cx| v.set_layout_size(i as u8, cx));
                    window.refresh();
                }),
        );
    }
    let slider = div()
        .relative()
        .mx_4()
        .h(px(24.))
        .child(
            // track, filled up to the knob
            div()
                .absolute()
                .left(px(9.))
                .right(px(9.))
                .top(px(10.))
                .h(px(4.))
                .rounded_full()
                .bg(p.pressed)
                .child(div().h_full().rounded_full().bg(track_color).w(relative(frac))),
        )
        .child(
            div()
                .absolute()
                .top(px(3.))
                .left(relative(frac * 0.94))
                .size(px(18.))
                .rounded_full()
                .bg(p.card_bg)
                .border_1()
                .border_color(p.border)
                .child(div().m(px(4.)).size(px(8.)).rounded_full().bg(track_color)),
        )
        .child(stops);
    let size_icons = h_flex().mx_4().mt_1().justify_between().text_color(if enabled { p.text_secondary } else { p.text_tertiary }).children(
        [12., 14., 16., 18., 20.].into_iter().map(|s| Icon::new(if mode == ViewMode::Details { L::Rows3 } else { L::Image }).size(px(s))),
    );

    // ---- switches
    let row = |label: &str, control: AnyElement| {
        h_flex().px_4().h(px(44.)).items_center().child(div().flex_1().text_sm().child(label.to_string())).child(control)
    };
    let w_hidden = weak.clone();
    let ext = s.show_extensions;
    let adaptive = s.default_view == crate::settings::DefaultView::Adaptive;
    let (d, sh) = (dual.get(), shelf.get());
    let dual2 = dual.clone();
    let shelf2 = shelf.clone();

    v_flex()
        .w_full()
        .pb_2()
        .child(section(p, t!("Sayfa düzeni")))
        .child(tiles)
        .child(divider(p))
        .child(section(p, t!("Boyut")).when(!enabled, |d| d.text_color(p.text_tertiary)))
        .child(slider)
        .child(size_icons)
        .child(divider(p))
        .child(row(
            t!("Gizli öğeler"),
            crate::settings_view::switch("lt-hidden", p, hidden, move |window, cx| {
                let _ = w_hidden.update(cx, |v, cx| v.toggle_hidden(cx));
                window.refresh();
            }),
        ))
        .child(row(
            t!("Dosya uzantıları"),
            crate::settings_view::switch("lt-ext", p, ext, move |window, cx| {
                crate::settings::update(cx, |s| s.show_extensions = !ext);
                window.refresh();
            }),
        ))
        .child(row(
            t!("Uyarlanabilir düzen"),
            crate::settings_view::switch("lt-adaptive", p, adaptive, move |window, cx| {
                crate::settings::update(cx, |s| {
                    s.default_view = if adaptive { crate::settings::DefaultView::Details } else { crate::settings::DefaultView::Adaptive }
                });
                window.refresh();
            }),
        ))
        .child(row(
            t!("Çift panel"),
            crate::settings_view::switch("lt-dual", p, d, move |window, cx| {
                dual2.set(!dual2.get());
                let _ = owner.update(cx, |_, w, cx| w.dispatch_action(Box::new(crate::actions::ToggleDualPane), cx));
                window.refresh();
            }),
        ))
        .child(row(
            t!("Raf"),
            crate::settings_view::switch("lt-shelf", p, sh, move |window, cx| {
                shelf2.set(!shelf2.get());
                let _ = owner.update(cx, |_, w, cx| w.dispatch_action(Box::new(crate::actions::ToggleShelf), cx));
                window.refresh();
            }),
        ))
        .into_any_element()
}
