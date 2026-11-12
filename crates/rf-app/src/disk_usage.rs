//! Disk usage page (Location::DiskUsage): where the space under a folder or drive goes.
//! The walk is [`rf_win::diskusage`] (below-normal priority, closing the tab stops it); this
//! file draws the result: the current folder's children as a list, biggest first, and as a
//! squarified treemap (folders show their own children inside them, one level deep).

use std::path::PathBuf;

use gpui_kit::assets::IconName as L;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rf_core::format;
use rf_core::treemap::{Rect, squarify};
use rf_win::diskusage::{DiskScan, DiskTree, Metric};

use crate::fluent_menu::{self, Item, Menu, MenuIcon};
use crate::icons;
use crate::t;
use crate::theme::{self, Palette};

/// Rows in the list (the biggest ones; the treemap shows the rest by area).
const LIST_MAX: usize = 400;
/// Rectangles smaller than this (either side) are not drawn.
const MIN_RECT: f32 = 3.;
/// Height of a folder's name strip inside the treemap.
const HEAD_H: f32 = 17.;
/// A folder rectangle shows its children when it is at least this big.
const NEST_MIN: (f32, f32) = (70., 46.);

/// Tab and breadcrumb title: "Disk kullanımı — Windows".
pub fn title(root: &std::path::Path) -> String {
    format!("{} — {}", t!("Disk kullanımı"), rf_core::history::display_name(root))
}

pub enum DiskEvent {
    /// Open this path's folder in a normal view with the item selected.
    ShowInFolder(PathBuf),
}

impl EventEmitter<DiskEvent> for DiskUsageView {}

/// One drawn rectangle of the treemap.
struct Cell {
    id: u32,
    rect: Rect,
    /// A folder's name strip (its children are separate cells inside it).
    head: bool,
    nested: bool,
}

pub struct DiskUsageView {
    root: PathBuf,
    scan: Option<DiskScan>,
    /// (entries, bytes) while walking.
    progress: (u64, u64),
    tree: Option<DiskTree>,
    cur: u32,
    metric: Metric,
    /// Children of `cur`, biggest first.
    kids: Vec<u32>,
    selected: Option<u32>,
    hover: Option<u32>,
    /// Size of the treemap area, measured while drawing.
    area: Size<Pixels>,
    cells: Option<((u32, Metric, i32, i32), Vec<Cell>)>,
    started: std::time::Instant,
    _poll: Option<Task<()>>,
}

impl DiskUsageView {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            scan: Some(DiskScan::start(root.clone())),
            root,
            progress: (0, 0),
            tree: None,
            cur: 0,
            metric: Metric::Logical,
            kids: Vec::new(),
            selected: None,
            hover: None,
            area: size(px(600.), px(400.)),
            cells: None,
            started: std::time::Instant::now(),
            _poll: None,
        };
        this.poll(cx);
        this
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Watches the walk: counters twice a second, the finished tree when it arrives.
    fn poll(&mut self, cx: &mut Context<Self>) {
        self._poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
                let Ok(done) = this.update(cx, |v, cx| {
                    let Some(scan) = &v.scan else { return true };
                    v.progress = scan.progress();
                    if let Some(t) = scan.take() {
                        v.tree = Some(t);
                        v.scan = None;
                        v.cur = 0;
                        v.refresh_kids();
                        cx.notify();
                        return true;
                    }
                    cx.notify();
                    false
                }) else {
                    return;
                };
                if done {
                    return;
                }
            }
        }));
    }

    fn refresh_kids(&mut self) {
        self.cells = None;
        self.kids = self.tree.as_ref().map(|t| t.sorted_children(self.cur, self.metric)).unwrap_or_default();
    }

    fn go(&mut self, id: u32, cx: &mut Context<Self>) {
        self.cur = id;
        self.selected = None;
        self.refresh_kids();
        cx.notify();
    }

    fn up(&mut self, cx: &mut Context<Self>) {
        if let Some(p) = self.tree.as_ref().and_then(|t| t.parent(self.cur)) {
            let from = self.cur;
            self.go(p, cx);
            self.selected = Some(from);
        }
    }

    fn set_metric(&mut self, m: Metric, cx: &mut Context<Self>) {
        if self.metric != m {
            self.metric = m;
            self.refresh_kids();
            cx.notify();
        }
    }

    /// Click on a row or rectangle: folders open, files get selected; a double click on a
    /// file opens it.
    fn activate(&mut self, id: u32, clicks: usize, cx: &mut Context<Self>) {
        let Some(t) = &self.tree else { return };
        if t.is_dir(id) {
            self.go(id, cx);
        } else if clicks >= 2 {
            crate::folder_view::open_in_background(t.path(id), cx);
        } else {
            self.selected = Some(id);
            cx.notify();
        }
    }

    fn show_in_folder(&mut self, id: u32, cx: &mut Context<Self>) {
        if let Some(t) = &self.tree {
            cx.emit(DiskEvent::ShowInFolder(t.path(id)));
        }
    }

    fn context_menu(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = &self.tree else { return };
        let is_dir = t.is_dir(id);
        self.selected = Some(id);
        cx.notify();
        let me = cx.entity().downgrade();
        let mut items = Vec::new();
        {
            let me = me.clone();
            let label = if is_dir { t!("Bu klasöre git") } else { t!("Aç") };
            items.push(Item::action(label, MenuIcon::Lucide(if is_dir { L::FolderOpen } else { L::SquareArrowOutUpRight }), move |_, cx| {
                let _ = me.update(cx, |v, cx| v.activate(id, 2, cx));
            }));
        }
        items.push(Item::action(t!("Klasörde göster"), MenuIcon::Lucide(L::FolderSearch), move |_, cx| {
            let _ = me.update(cx, |v, cx| v.show_in_folder(id, cx));
        }));
        fluent_menu::show(Menu { quick: Vec::new(), items }, window.mouse_position(), window, cx);
    }

    // ---------- treemap layout

    /// Rectangles for the current folder inside `w` x `h`.
    fn layout(&mut self, w: f32, h: f32) -> &[Cell] {
        let key = (self.cur, self.metric, w as i32, h as i32);
        if self.cells.as_ref().is_none_or(|(k, _)| *k != key) {
            let cells = self.tree.as_ref().map(|t| build_cells(t, self.metric, &self.kids, w, h)).unwrap_or_default();
            self.cells = Some((key, cells));
        }
        self.cells.as_ref().map(|(_, c)| c.as_slice()).unwrap_or(&[])
    }
}

fn build_cells(t: &DiskTree, m: Metric, kids: &[u32], w: f32, h: f32) -> Vec<Cell> {
    let mut out = Vec::new();
    let weights: Vec<u64> = kids.iter().map(|&k| t.weight(k, m)).collect();
    let rects = squarify(&weights, Rect::new(0., 0., w, h));
    for (&id, r) in kids.iter().zip(rects) {
        if r.w < MIN_RECT || r.h < MIN_RECT {
            continue;
        }
        let r = r.inset(1.);
        out.push(Cell { id, rect: r, head: false, nested: false });
        if !t.is_dir(id) || r.w < NEST_MIN.0 || r.h < NEST_MIN.1 {
            continue;
        }
        // the folder's children inside it, below its name strip
        let inner = Rect::new(r.x + 3., r.y + HEAD_H, (r.w - 6.).max(0.), (r.h - HEAD_H - 3.).max(0.));
        let sub = t.sorted_children(id, m);
        let sw: Vec<u64> = sub.iter().map(|&k| t.weight(k, m)).collect();
        out.push(Cell { id, rect: Rect::new(r.x, r.y, r.w, HEAD_H), head: true, nested: false });
        for (&c, cr) in sub.iter().zip(squarify(&sw, inner)) {
            if cr.w >= MIN_RECT && cr.h >= MIN_RECT {
                out.push(Cell { id: c, rect: cr.inset(0.5), head: false, nested: true });
            }
        }
    }
    out
}

fn category_of(t: &DiskTree, id: u32) -> icons::Category {
    if t.is_dir(id) {
        return icons::Category::Folder;
    }
    let name = t.name(id);
    name.rsplit_once('.').map_or(icons::Category::Other, |(_, e)| icons::category_for_ext(&e.to_ascii_lowercase()))
}

fn count(n: u64) -> String {
    crate::bench_view::count(n)
}

impl Render for DiskUsageView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let Some(t) = self.tree.as_ref() else {
            return self.render_scanning(&p).into_any_element();
        };
        let metric = self.metric;
        let cur = self.cur;
        let total = t.weight(cur, metric).max(1);

        // ---- header: up button, path, size, metric switch
        let mut crumbs = Vec::new();
        let mut c = cur;
        loop {
            crumbs.push(c);
            match t.parent(c) {
                Some(par) => c = par,
                None => break,
            }
        }
        crumbs.reverse();
        let root_name = {
            let n = rf_core::history::display_name(&self.root);
            if n.is_empty() { self.root.to_string_lossy().into_owned() } else { n }
        };
        let mut path_row = h_flex().gap_1().items_center().min_w_0().overflow_hidden();
        for (i, &c) in crumbs.iter().enumerate() {
            let label = if i == 0 { root_name.clone() } else { t.name(c).to_string() };
            let last = i + 1 == crumbs.len();
            if i > 0 {
                path_row = path_row.child(Icon::new(L::ChevronRight).size(px(12.)).text_color(p.text_tertiary));
            }
            path_row = path_row.child(
                div()
                    .id(("crumb", i))
                    .px_1p5()
                    .py_0p5()
                    .rounded(theme::RADIUS_SM)
                    .text_sm()
                    .whitespace_nowrap()
                    .when(last, |d| d.font_weight(FontWeight::SEMIBOLD).text_color(p.text))
                    .when(!last, |d| d.text_color(p.text_secondary).cursor_pointer().hover(|s| s.bg(p.hover)).on_click(cx.listener(move |v, _, _, cx| v.go(c, cx))))
                    .child(label),
            );
        }
        let can_up = t.parent(cur).is_some();
        let up_btn = div()
            .id("du-up")
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::RADIUS_SM)
            .when(can_up, |d| d.cursor_pointer().hover(|s| s.bg(p.hover)).on_click(cx.listener(|v, _, _, cx| v.up(cx))))
            .child(Icon::new(L::ArrowUp).size(px(16.)).text_color(if can_up { p.text } else { p.text_tertiary }));
        let seg = |id: &'static str, label: &'static str, m: Metric| {
            let on = metric == m;
            div()
                .id(id)
                .px_2()
                .py_0p5()
                .rounded(theme::RADIUS_SM)
                .text_xs()
                .cursor_pointer()
                .when(on, |d| d.bg(p.selection).text_color(p.text))
                .when(!on, |d| d.text_color(p.text_secondary).hover(|s| s.bg(p.hover)))
                .on_click(cx.listener(move |v, _, _, cx| v.set_metric(m, cx)))
                .child(label)
        };
        let summary = format!(
            "{} · {}",
            format::size(t.weight(cur, metric)),
            format!("{} {}", count(t.files(cur) as u64), t!("dosya"))
        );
        let header = h_flex()
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(p.divider)
            .child(up_btn)
            .child(div().flex_1().min_w_0().child(path_row))
            .child(div().text_xs().text_color(p.text_secondary).whitespace_nowrap().child(summary))
            .child(
                h_flex()
                    .gap_0p5()
                    .p_0p5()
                    .rounded(theme::RADIUS_SM)
                    .bg(p.pressed)
                    .child(seg("m-logical", t!("Boyut"), Metric::Logical))
                    .child(seg("m-disk", t!("Diskteki boyut"), Metric::OnDisk)),
            );

        // ---- list
        let mut list = v_flex().id("du-list").flex_1().min_h_0().overflow_y_scroll().p_1();
        for (i, &id) in self.kids.iter().take(LIST_MAX).enumerate() {
            let w = t.weight(id, metric);
            let frac = w as f32 / total as f32;
            let cat = category_of(t, id);
            let is_dir = t.is_dir(id);
            let sel = self.selected == Some(id);
            let hov = self.hover == Some(id);
            list = list.child(
                h_flex()
                    .id(("du-row", i))
                    .relative()
                    .h(theme::ROW_H)
                    .px_2()
                    .gap_2()
                    .items_center()
                    .flex_shrink_0()
                    .rounded(theme::RADIUS_SM)
                    .text_sm()
                    .cursor_pointer()
                    .when(sel, |d| d.bg(p.selection).child(theme::selection_pill(&p)))
                    .when(!sel && hov, |d| d.bg(p.hover))
                    .on_hover(cx.listener(move |v, on: &bool, _, cx| {
                        let want = on.then_some(id);
                        if *on && v.hover != want || !*on && v.hover == Some(id) {
                            v.hover = want;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |v, ev: &ClickEvent, _, cx| v.activate(id, ev.click_count(), cx)))
                    .on_mouse_down(MouseButton::Right, cx.listener(move |v, _, window, cx| v.context_menu(id, window, cx)))
                    .child(Icon::new(icons::icon_for(cat)).size(px(16.)).text_color(theme::kind_color(cat, p.dark)))
                    .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(t.name(id).to_string()))
                    .child(div().w(px(76.)).flex_shrink_0().text_right().text_color(p.text_secondary).child(format::size(w)))
                    .child(
                        div()
                            .w(px(64.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(div().h(px(4.)).flex_1().rounded_full().bg(p.pressed).child(div().h_full().rounded_full().bg(p.accent).w(relative(frac.clamp(0.0, 1.0)))))
                            .child(div().w(px(34.)).text_xs().text_right().text_color(p.text_tertiary).child(crate::i18n::pct(frac * 100.))),
                    )
                    .child(div().w(px(54.)).flex_shrink_0().text_xs().text_right().text_color(p.text_tertiary).when(is_dir, |d| d.child(count(t.files(id) as u64)))),
            );
        }
        let hidden = self.kids.len().saturating_sub(LIST_MAX);
        if hidden > 0 {
            list = list.child(div().px_2().py_1().text_xs().text_color(p.text_tertiary).child(format!("+{} {}", count(hidden as u64), t!("öğe daha"))));
        }
        if self.kids.is_empty() {
            list = list.child(div().p_4().text_sm().text_color(p.text_tertiary).child(t!("Bu klasör boş")));
        }

        // ---- treemap
        let (aw, ah) = (f32::from(self.area.width), f32::from(self.area.height));
        let hover = self.hover;
        let selected = self.selected;
        let dark = p.dark;
        let folder_bg = if dark { hsla(0., 0., 1., 0.07) } else { hsla(0., 0., 0., 0.06) };
        let folder_head = if dark { hsla(0., 0., 1., 0.12) } else { hsla(0., 0., 0., 0.10) };
        let cells: Vec<(u32, Rect, bool, bool)> = self.layout(aw, ah).iter().map(|c| (c.id, c.rect, c.head, c.nested)).collect();
        let t = self.tree.as_ref().expect("tree");
        let mut map = div().relative().size_full().overflow_hidden();
        for (i, &(id, r, head, nested)) in cells.iter().enumerate() {
            let is_dir = t.is_dir(id);
            let cat = category_of(t, id);
            let lit = hover == Some(id) || selected == Some(id);
            let base = if is_dir {
                if head { folder_head } else { folder_bg }
            } else {
                let mut c = theme::kind_color(cat, dark);
                c.a = if lit { 1.0 } else { 0.82 };
                c
            };
            let mut cell = div()
                .id(("du-cell", i))
                .absolute()
                .left(px(r.x))
                .top(px(r.y))
                .w(px(r.w))
                .h(px(r.h))
                .rounded(px(if r.w > 14. && r.h > 14. { 4. } else { 1.5 }))
                .overflow_hidden()
                .bg(base)
                .when(is_dir && !head && !nested, |d| d.border_1().border_color(p.border))
                .when(lit && !head, |d| d.border_1().border_color(p.text_secondary))
                .cursor_pointer()
                .on_hover(cx.listener(move |v, on: &bool, _, cx| {
                    let want = on.then_some(id);
                    if *on && v.hover != want || !*on && v.hover == Some(id) {
                        v.hover = want;
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(move |v, ev: &ClickEvent, _, cx| v.activate(id, ev.click_count(), cx)))
                .on_mouse_down(MouseButton::Right, cx.listener(move |v, _, window, cx| v.context_menu(id, window, cx)));
            // a folder's body sits behind its children: only its strip and the empty part take
            // clicks, which is the same folder either way
            let label_color = if is_dir { p.text } else { gpui::white() };
            if head {
                cell = cell.px_1p5().flex().items_center().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(label_color).whitespace_nowrap().child(div().overflow_hidden().text_ellipsis().child(format!("{}  {}", t.name(id), format::size(t.weight(id, metric)))));
            } else if !is_dir && r.w > 44. && r.h > 20. {
                let big = r.w > 80. && r.h > 36.;
                cell = cell.p_1().text_xs().text_color(label_color).child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().child(t.name(id).to_string())).when(big, |d| {
                    d.child(div().overflow_hidden().whitespace_nowrap().opacity(0.8).child(format::size(t.weight(id, metric))))
                });
            } else if is_dir && !head && !cells.iter().any(|&(i2, _, h2, _)| h2 && i2 == id) && r.w > 44. && r.h > 20. {
                // a folder without room for its children (or one inside another folder): just its name
                cell = cell.p_1().text_xs().text_color(label_color).child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().child(t.name(id).to_string()));
            }
            map = map.child(cell);
        }
        let this = cx.entity();
        let probe = canvas(
            move |bounds, window, cx| {
                let changed = this.update(cx, |v, cx| {
                    let ch = v.area != bounds.size;
                    if ch {
                        v.area = bounds.size;
                        cx.notify();
                    }
                    ch
                });
                if changed {
                    window.request_animation_frame();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        // ---- footer: what is under the pointer
        let focus = hover.or(selected).unwrap_or(cur);
        let foot = {
            let path = t.path(focus);
            let mut s = format!("{}  ·  {}", path.to_string_lossy(), format::size(t.weight(focus, metric)));
            if t.is_dir(focus) {
                s.push_str(&format!("  ·  {}", format!("{} {}", count(t.files(focus) as u64), t!("dosya"))));
            }
            if t.skipped() > 0 && focus == 0 {
                s.push_str(&format!("  ·  {} {}", t.skipped(), t!("klasör okunamadı")));
            }
            s
        };

        v_flex()
            .size_full()
            .child(header)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(v_flex().w(px(430.)).flex_shrink_0().h_full().border_r_1().border_color(p.divider).child(list))
                    .child(div().relative().flex_1().min_w_0().h_full().p_2().child(div().relative().size_full().child(probe).child(map))),
            )
            .child(
                div()
                    .px_3()
                    .py_1p5()
                    .border_t_1()
                    .border_color(p.divider)
                    .text_xs()
                    .text_color(p.text_secondary)
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(foot),
            )
            .into_any_element()
    }
}

impl DiskUsageView {
    fn render_scanning(&self, p: &Palette) -> impl IntoElement {
        let (entries, bytes) = self.progress;
        let secs = self.started.elapsed().as_secs();
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(Icon::new(L::ChartPie).size(px(40.)).text_color(p.accent))
            .child(div().text_color(p.text).child(t!("Disk kullanımı hesaplanıyor…")))
            .child(div().text_sm().text_color(p.text_secondary).child(format!("{} {} · {} · {} s", count(entries), t!("öğe"), format::size(bytes), secs)))
            .child(div().text_xs().text_color(p.text_tertiary).child(t!("Sekmeyi kapatmak taramayı durdurur")))
    }
}
