//! Select box: a bordered field showing the current value with up/down chevrons; its options
//! open in an Acrylic popup right under it (the material of the context menus), at least as
//! wide as the field, the chosen one ticked on the right. More than [`SEARCH_FROM`] options get
//! a search box. Keys: ↑ ↓, PgUp/PgDn, Home/End, Enter, Esc. Clicking elsewhere closes it.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as L;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::t;
use crate::theme::{self, Palette};

/// More options than this: a search box on top.
pub const SEARCH_FROM: usize = 10;
const ROW_H: f32 = 32.;
const PAD: f32 = 4.;
const SEARCH_H: f32 = 40.;
const LIST_MAX: f32 = 320.;
const EDGE: f32 = 8.;
const PAGE: usize = 8;

pub struct Choice {
    pub label: SharedString,
    pub selected: bool,
    pub run: Rc<dyn Fn(&mut Window, &mut App)>,
}

impl Choice {
    pub fn new(label: impl Into<SharedString>, selected: bool, run: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        Self { label: label.into(), selected, run: Rc::new(run) }
    }
}

/// The open popup (one at a time), and which field closed one last: a click on the field that
/// owns the open popup first deactivates the popup (closing it); that same click must not
/// reopen it.
#[derive(Default)]
struct Host {
    open: Option<AnyWindowHandle>,
    closed: Option<(SharedString, Instant)>,
}
impl Global for Host {}

/// The field. `choices` is asked when it is clicked.
pub fn select_box(id: impl Into<SharedString>, p: &Palette, label: impl Into<SharedString>, choices: impl Fn(&App) -> Vec<Choice> + 'static) -> AnyElement {
    select_box_h(id, p, label, 32., choices)
}

/// [`select_box`] at a given height (to line up with other controls in a row).
pub fn select_box_h(id: impl Into<SharedString>, p: &Palette, label: impl Into<SharedString>, height: f32, choices: impl Fn(&App) -> Vec<Choice> + 'static) -> AnyElement {
    let key: SharedString = id.into();
    let bounds = Rc::new(Cell::new(Bounds::<Pixels>::default()));
    let seen = bounds.clone();
    h_flex()
        .id(ElementId::Name(key.clone()))
        .relative()
        .h(px(height))
        .min_w(px(180.))
        .max_w(px(320.))
        .px_3()
        .gap_2()
        .items_center()
        .rounded(theme::RADIUS_SM)
        .bg(p.hover)
        .border_1()
        .border_color(p.border)
        .text_sm()
        .cursor_pointer()
        .hover(|s| s.border_color(p.text_tertiary))
        .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(label.into()))
        .child(Icon::new(L::ChevronsUpDown).size(px(14.)).text_color(p.text_secondary))
        .child(canvas(move |b, _, _| seen.set(b), |_, _, _, _| {}).absolute().top_0().left_0().size_full())
        .on_click(move |_, window, cx| {
            let key = key.clone();
            let just_closed = cx
                .try_global::<Host>()
                .and_then(|h| h.closed.clone())
                .is_some_and(|(k, at)| k == key && at.elapsed() < Duration::from_millis(250));
            if !just_closed {
                open(key, bounds.get(), choices(cx), window, cx);
            }
        })
        .into_any_element()
}

/// Turkish-insensitive search key: case, dotted/dotless i and common accents don't matter.
fn fold(s: &str) -> String {
    s.chars()
        .flat_map(|c| c.to_lowercase())
        .map(|c| match c {
            'ı' | 'í' | 'ì' | 'î' | 'ï' => 'i',
            'ç' | 'ć' | 'č' => 'c',
            'ğ' => 'g',
            'ö' | 'ó' | 'ò' | 'ô' | 'õ' | 'ø' => 'o',
            'ş' | 'ś' | 'š' => 's',
            'ü' | 'ú' | 'ù' | 'û' => 'u',
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'ñ' | 'ń' => 'n',
            'ž' | 'ź' | 'ż' => 'z',
            other => other,
        })
        .filter(|c| *c != '\u{307}') // the dot left over from lowercasing "İ"
        .collect()
}

/// Indices of `labels` matching `query`: prefix matches first, then the rest that contain it.
fn filter(labels: &[SharedString], query: &str) -> Vec<usize> {
    let q = fold(query.trim());
    if q.is_empty() {
        return (0..labels.len()).collect();
    }
    let (mut starts, mut contains) = (Vec::new(), Vec::new());
    for (i, l) in labels.iter().enumerate() {
        let f = fold(l);
        if f.starts_with(&q) {
            starts.push(i);
        } else if f.contains(&q) {
            contains.push(i);
        }
    }
    starts.extend(contains);
    starts
}

fn open(key: SharedString, field: Bounds<Pixels>, choices: Vec<Choice>, window: &mut Window, cx: &mut App) {
    if choices.is_empty() {
        return;
    }
    close_open(cx);
    let search = choices.len() > SEARCH_FROM;
    // wide enough for the longest label (estimated: the popup is sized before it is drawn)
    let longest = choices.iter().map(|c| c.label.chars().count()).max().unwrap_or(0) as f32;
    let fw = f32::from(field.size.width);
    let w = (longest * 7.4 + 64.).clamp(fw.max(180.), fw.max(440.));
    let list_h = (choices.len() as f32 * ROW_H).min(LIST_MAX);
    let h = list_h + 2. * PAD + if search { SEARCH_H } else { 0. };

    let win = window.bounds();
    let area = window.display(cx).map(|d| d.bounds()).unwrap_or(win);
    let (ax, ay) = (f32::from(area.origin.x), f32::from(area.origin.y));
    let (right, bottom) = (ax + f32::from(area.size.width), ay + f32::from(area.size.height));
    let fx = f32::from(win.origin.x + field.origin.x);
    let ftop = f32::from(win.origin.y + field.origin.y);
    let fbottom = ftop + f32::from(field.size.height);
    let x = fx.clamp(ax + EDGE, (right - EDGE - w).max(ax + EDGE));
    let y = if fbottom + 4. + h <= bottom - EDGE { fbottom + 4. } else { (ftop - 4. - h).max(ay + EDGE) };

    let dark = match crate::settings::theme_override() {
        crate::settings::ThemePref::Light => false,
        crate::settings::ThemePref::Dark => true,
        crate::settings::ThemePref::System => matches!(cx.window_appearance(), WindowAppearance::Dark | WindowAppearance::VibrantDark),
    };
    let main = window.window_handle();
    let opts = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(x), px(y)), size(px(w), px(h))))),
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
    let opened = cx.open_window(opts, move |w, cx| {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(h) = w.window_handle() {
            if let RawWindowHandle::Win32(h) = h.as_raw() {
                rf_win::window::set_dark_mode(h.hwnd.get(), dark);
            }
        }
        cx.new(|cx| SelectView::new(key, choices, search, main, w, cx))
    });
    match opened {
        Ok(h) => {
            // the app window keeps its active look while the popup has the keyboard
            gpui_windows::KEEP_ACTIVE_LOOK.store(true, std::sync::atomic::Ordering::Relaxed);
            cx.default_global::<Host>().open = Some(h.into());
        }
        Err(e) => log::warn!("select popup: {e:#}"),
    }
}

/// Closes the open popup (if any), after the current update.
fn close_open(cx: &mut App) {
    let Some(w) = cx.default_global::<Host>().open.take() else { return };
    gpui_windows::KEEP_ACTIVE_LOOK.store(false, std::sync::atomic::Ordering::Relaxed);
    cx.defer(move |cx| {
        let _ = w.update(cx, |_, window, _| window.remove_window());
    });
}

struct SelectView {
    key: SharedString,
    choices: Rc<Vec<Choice>>,
    labels: Vec<SharedString>,
    /// Indices into `choices`, in display order.
    shown: Vec<usize>,
    /// Highlighted position in `shown`.
    hl: usize,
    search: Option<Entity<InputState>>,
    focus: FocusHandle,
    scroll: ScrollHandle,
    main: AnyWindowHandle,
    done: bool,
    _subs: Vec<Subscription>,
}

impl SelectView {
    fn new(key: SharedString, choices: Vec<Choice>, search: bool, main: AnyWindowHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let labels: Vec<SharedString> = choices.iter().map(|c| c.label.clone()).collect();
        let hl = choices.iter().position(|c| c.selected).unwrap_or(0);
        let focus = cx.focus_handle();
        let mut subs = vec![cx.observe_window_activation(window, |this: &mut Self, window, cx| {
            if !window.is_window_active() {
                this.dismiss(cx);
            }
        })];
        let search = search.then(|| {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder(t!("Ara")));
            subs.push(cx.subscribe_in(&input, window, |this: &mut Self, input, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    let q = input.read(cx).value().to_string();
                    this.shown = filter(&this.labels, &q);
                    this.hl = 0;
                    this.scroll.scroll_to_item(0);
                    cx.notify();
                }
            }));
            input
        });
        match &search {
            Some(input) => input.update(cx, |s, cx| s.focus(window, cx)),
            None => window.focus(&focus, cx),
        }
        let scroll = ScrollHandle::new();
        scroll.scroll_to_item(hl);
        Self { key, shown: (0..choices.len()).collect(), choices: Rc::new(choices), labels, hl, search, focus, scroll, main, done: false, _subs: subs }
    }

    /// Closes without choosing (Esc, a click elsewhere).
    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.done {
            return;
        }
        self.done = true;
        cx.default_global::<Host>().closed = Some((self.key.clone(), Instant::now()));
        close_open(cx);
    }

    fn pick(&mut self, pos: usize, cx: &mut Context<Self>) {
        let Some(&ix) = self.shown.get(pos) else { return };
        let run = self.choices[ix].run.clone();
        let main = self.main;
        self.dismiss(cx);
        cx.defer(move |cx| {
            let _ = main.update(cx, |_, window, cx| {
                window.activate_window();
                run(window, cx);
                window.refresh();
            });
        });
    }

    fn move_hl(&mut self, to: usize, cx: &mut Context<Self>) {
        if self.shown.is_empty() {
            return;
        }
        self.hl = to.min(self.shown.len() - 1);
        self.scroll.scroll_to_item(self.hl);
        cx.notify();
    }

    fn on_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let n = self.shown.len();
        match ev.keystroke.key.as_str() {
            "escape" => self.dismiss(cx),
            "enter" => self.pick(self.hl, cx),
            "down" => self.move_hl(if self.hl + 1 < n { self.hl + 1 } else { 0 }, cx),
            "up" => self.move_hl(if self.hl > 0 { self.hl - 1 } else { n.saturating_sub(1) }, cx),
            "pagedown" => self.move_hl(self.hl + PAGE, cx),
            "pageup" => self.move_hl(self.hl.saturating_sub(PAGE), cx),
            // with a search box, Home/End move in the text
            "home" if self.search.is_none() => self.move_hl(0, cx),
            "end" if self.search.is_none() => self.move_hl(n.saturating_sub(1), cx),
            _ => return,
        }
        cx.stop_propagation();
    }
}

impl Render for SelectView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let mut list = v_flex().id("select-list").flex_1().min_h_0().overflow_y_scroll().track_scroll(&self.scroll);
        if self.shown.is_empty() {
            list = list.child(div().h(px(ROW_H)).flex().items_center().justify_center().text_sm().text_color(p.text_secondary).child(t!("Eşleşen öğe yok")));
        }
        for (pos, &ix) in self.shown.iter().enumerate() {
            let c = &self.choices[ix];
            list = list.child(
                h_flex()
                    .id(("opt", ix))
                    .flex_shrink_0()
                    .h(px(ROW_H))
                    .px_3()
                    .gap_2()
                    .items_center()
                    .rounded(px(4.))
                    .text_sm()
                    .when(pos == self.hl, |d| d.bg(p.hover))
                    .active(|s| s.bg(p.pressed))
                    .on_hover(cx.listener(move |this, on: &bool, _, cx| {
                        if *on && this.hl != pos {
                            this.hl = pos;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| this.pick(pos, cx)))
                    .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(c.label.clone()))
                    .child(div().w(px(16.)).flex_shrink_0().when(c.selected, |d| d.child(Icon::new(L::Check).size(px(16.)).text_color(p.accent)))),
            );
        }
        v_flex()
            .size_full()
            .p(px(PAD))
            .text_color(p.text)
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| this.on_key(ev, cx)))
            .children(self.search.as_ref().map(|input| {
                div()
                    .flex_shrink_0()
                    .h(px(SEARCH_H - PAD))
                    .mb(px(PAD))
                    .child(Input::new(input).prefix(Icon::new(L::Search).size(px(14.)).text_color(p.text_secondary)))
            }))
            .child(list)
    }
}

#[cfg(test)]
mod tests {
    use super::{filter, fold};
    use gpui_kit::SharedString;

    #[test]
    fn search_ignores_case_and_turkish_letters() {
        assert_eq!(fold("İngilizce"), "ingilizce");
        assert_eq!(fold("Türkçe"), "turkce");
        assert_eq!(fold("Français"), "francais");
        let labels: Vec<SharedString> = ["English", "Türkçe", "Deutsch", "Português (Brasil)"].into_iter().map(Into::into).collect();
        assert_eq!(filter(&labels, "tur"), vec![1]);
        assert_eq!(filter(&labels, "SC"), vec![2]); // contains
        assert_eq!(filter(&labels, "p"), vec![3]);
        assert_eq!(filter(&labels, ""), vec![0, 1, 2, 3]);
        assert_eq!(filter(&labels, "e"), vec![0, 1, 2, 3]); // "English" starts, the rest contain
    }
}
