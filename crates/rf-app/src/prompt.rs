//! A small Fluent dialog (title, message, optional text box, buttons) drawn by the app, so
//! questions look like the rest of smoothyfiles instead of a Win32 message box. One at a time;
//! the workspace of its window paints it on top.

use std::rc::Rc;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::theme::{self, Palette};

/// Button handler: gets the text box value (if any). `Err(message)` keeps the dialog open and
/// shows the message under the text box (invalid input).
pub type OnButton = Rc<dyn Fn(Option<String>, &mut Window, &mut App) -> Result<(), String>>;

pub struct Button {
    pub label: SharedString,
    pub kind: Kind,
    pub run: OnButton,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Primary,
    Normal,
    Danger,
}

impl Button {
    pub fn new(label: impl Into<SharedString>, kind: Kind, run: impl Fn(Option<String>, &mut Window, &mut App) -> Result<(), String> + 'static) -> Self {
        Self { label: label.into(), kind, run: Rc::new(run) }
    }
    /// A button that just closes.
    pub fn cancel() -> Self {
        Self::new(crate::t!("İptal"), Kind::Normal, |_, _, _| Ok(()))
    }
}

/// A labelled row of mutually exclusive options (segmented control). The caller keeps the
/// `selected` cell and reads it in its button handler.
pub struct Choice {
    pub label: SharedString,
    pub options: Vec<SharedString>,
    pub selected: Rc<std::cell::Cell<usize>>,
}

pub struct DialogView {
    title: SharedString,
    message: Option<SharedString>,
    error: Option<SharedString>,
    input: Option<Entity<InputState>>,
    choices: Vec<Choice>,
    buttons: Vec<Button>,
    focus: FocusHandle,
    _sub: Option<Subscription>,
}

struct Host(Option<(AnyWindowHandle, Entity<DialogView>, Option<FocusHandle>)>);
impl Global for Host {}

/// Shows a dialog. `input = Some(initial)` adds a text box (Enter = first Primary button).
pub fn show(
    title: impl Into<SharedString>,
    message: Option<String>,
    input: Option<(String, String)>,
    buttons: Vec<Button>,
    window: &mut Window,
    cx: &mut App,
) {
    show_with(title, message, input, Vec::new(), buttons, window, cx)
}

/// `show` plus option rows under the text box.
pub fn show_with(
    title: impl Into<SharedString>,
    message: Option<String>,
    input: Option<(String, String)>,
    choices: Vec<Choice>,
    buttons: Vec<Button>,
    window: &mut Window,
    cx: &mut App,
) {
    show_inner(title, message, input, false, choices, buttons, window, cx)
}

/// A dialog whose text box hides what is typed (passwords).
pub fn show_secret(title: impl Into<SharedString>, message: Option<String>, placeholder: String, buttons: Vec<Button>, window: &mut Window, cx: &mut App) {
    show_inner(title, message, Some((String::new(), placeholder)), true, Vec::new(), buttons, window, cx)
}

#[allow(clippy::too_many_arguments)]
fn show_inner(
    title: impl Into<SharedString>,
    message: Option<String>,
    input: Option<(String, String)>,
    masked: bool,
    choices: Vec<Choice>,
    buttons: Vec<Button>,
    window: &mut Window,
    cx: &mut App,
) {
    let restore = window.focused(cx);
    let view = cx.new(|cx| {
        let input = input.map(|(initial, placeholder)| {
            cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder(placeholder).masked(masked);
                s.set_value(initial, window, cx);
                s
            })
        });
        let sub = input.as_ref().map(|i| {
            cx.subscribe_in(i, window, |this: &mut DialogView, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.press_primary(window, cx);
                } else if let InputEvent::Change = ev {
                    if this.error.take().is_some() {
                        cx.notify();
                    }
                }
            })
        });
        DialogView {
            title: title.into(),
            message: message.map(Into::into),
            error: None,
            input,
            choices,
            buttons,
            focus: cx.focus_handle(),
            _sub: sub,
        }
    });
    match view.read(cx).input.clone() {
        Some(i) => i.update(cx, |s, cx| s.focus(window, cx)),
        None => window.focus(&view.read(cx).focus.clone(), cx),
    }
    cx.set_global(Host(Some((window.window_handle(), view, restore))));
    window.refresh();
}

pub fn current(window: &Window, cx: &App) -> Option<Entity<DialogView>> {
    let (handle, view, _) = cx.try_global::<Host>()?.0.as_ref()?;
    (*handle == window.window_handle()).then(|| view.clone())
}

pub fn close(window: &mut Window, cx: &mut App) {
    let Some(Some((_, _, restore))) = cx.try_global::<Host>().map(|h| h.0.clone()) else { return };
    cx.set_global(Host(None));
    if let Some(f) = restore {
        window.focus(&f, cx);
    }
    window.refresh();
}

impl DialogView {
    fn value(&self, cx: &App) -> Option<String> {
        self.input.as_ref().map(|i| i.read(cx).value().to_string())
    }

    fn press(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.buttons.get(ix) else { return };
        let (run, value) = (b.run.clone(), self.value(cx));
        match run(value, window, cx) {
            Ok(()) => close(window, cx),
            Err(e) => {
                self.error = Some(e.into());
                cx.notify();
            }
        }
    }

    fn press_primary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.buttons.iter().position(|b| b.kind == Kind::Primary) {
            self.press(ix, window, cx);
        }
    }
}

impl Render for DialogView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let mut buttons = h_flex().gap_2().justify_end();
        for (ix, b) in self.buttons.iter().enumerate() {
            let (bg, fg) = match b.kind {
                Kind::Primary => (p.accent, p.accent_text),
                Kind::Danger => (p.danger, p.accent_text),
                Kind::Normal => (p.hover, p.text),
            };
            buttons = buttons.child(
                div()
                    .id(("dlg-btn", ix))
                    .min_w(px(96.))
                    .h(px(32.))
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::RADIUS_SM)
                    .text_sm()
                    .bg(bg)
                    .text_color(fg)
                    .when(b.kind == Kind::Normal, |d| d.border_1().border_color(p.border))
                    .hover(|s| s.opacity(0.9))
                    .active(|s| s.opacity(0.8))
                    .child(b.label.clone())
                    .on_click(cx.listener(move |this, _, window, cx| this.press(ix, window, cx))),
            );
        }
        let mut choice_rows = Vec::new();
        for (ci, c) in self.choices.iter().enumerate() {
            let mut seg = h_flex().p(px(2.)).gap(px(2.)).rounded(theme::RADIUS_SM).bg(p.content_bg).border_1().border_color(p.border);
            let cur = c.selected.get();
            for (oi, o) in c.options.iter().enumerate() {
                let on = oi == cur;
                let cell = c.selected.clone();
                seg = seg.child(
                    div()
                        .id(SharedString::from(format!("dlg-ch-{ci}-{oi}")))
                        .flex_1()
                        .h(px(28.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.))
                        .text_sm()
                        .when(on, |d| d.bg(p.card_bg).shadow_sm().font_weight(FontWeight::MEDIUM))
                        .when(!on, |d| d.text_color(p.text_secondary).hover(|s| s.bg(p.hover)))
                        .child(o.clone())
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cell.set(oi);
                            cx.notify();
                        })),
                );
            }
            choice_rows.push(
                v_flex()
                    .gap_1()
                    .child(div().text_xs().text_color(p.text_secondary).child(c.label.clone()))
                    .child(seg),
            );
        }
        div()
            .id("dialog-layer")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(hsla(0., 0., 0., 0.35))
            .track_focus(&self.focus)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| match ev.keystroke.key.as_str() {
                "escape" => {
                    close(window, cx);
                    cx.stop_propagation();
                }
                "enter" if this.input.is_none() => {
                    this.press_primary(window, cx);
                    cx.stop_propagation();
                }
                _ => {}
            }))
            .child(
                v_flex()
                    .w(px(440.))
                    .rounded(theme::RADIUS)
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.border)
                    .shadow_lg()
                    .overflow_hidden()
                    .text_color(p.text)
                    .child(
                        v_flex()
                            .p_6()
                            .gap_3()
                            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(self.title.clone()))
                            .children(self.message.clone().map(|m| div().text_sm().text_color(p.text_secondary).child(m)))
                            .children(self.input.as_ref().map(|i| {
                                div()
                                    .h(px(34.))
                                    .px_2()
                                    .flex()
                                    .items_center()
                                    .rounded(theme::RADIUS_SM)
                                    .bg(p.content_bg)
                                    .border_1()
                                    .border_color(if self.error.is_some() { p.danger } else { p.accent })
                                    .child(Input::new(i).appearance(false))
                            }))
                            .children(self.error.clone().map(|e| div().text_xs().text_color(p.danger).child(e)))
                            .children(choice_rows),
                    )
                    .child(div().px_6().py_4().bg(p.hover).border_t_1().border_color(p.divider).child(buttons))
                    .with_animation("dialog-in", Animation::new(std::time::Duration::from_millis(160)).with_easing(ease_out_quint()), |el, t| {
                        el.opacity(t).mt(px(10. * (1. - t)))
                    }),
            )
    }
}
