//! Bulk rename dialog card (Files' "Bulk rename", improved): find & replace (plain/regex),
//! pattern with counter, case change, prefix/suffix. Live preview with conflict highlighting;
//! Apply is disabled while any conflict exists. Renames run on a background thread using a
//! swap-safe two-phase scheme (`rf_core::rename::plan`).
//!
//! Embed: `cx.new(|cx| BulkRename::new(paths, window, cx))`, subscribe to [`BulkRenameEvent::Close`].
//! The view renders only the 720x520 card; the caller positions it as an overlay.
#![allow(dead_code)]

use crate::t;
use std::collections::HashSet;
use std::path::PathBuf;

use gpui_kit::assets::IconName as L;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rf_core::rename::{self, CaseKind, Mode, RenameError, RenameItem, RenameSpec};

use crate::theme::{self, Palette};

pub enum BulkRenameEvent {
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Find,
    Pattern,
    Case,
    Affix,
}

pub struct BulkRename {
    dir: PathBuf,
    names: Vec<String>,
    items: Vec<RenameItem>,
    siblings: Vec<String>,
    tab: Tab,
    find: Entity<InputState>,
    replace: Entity<InputState>,
    pattern: Entity<InputState>,
    start: Entity<InputState>,
    step: Entity<InputState>,
    prefix: Entity<InputState>,
    suffix: Entity<InputState>,
    case_insensitive: bool,
    regex: bool,
    include_ext: bool,
    case_kind: CaseKind,
    results: Vec<Result<String, RenameError>>,
    applying: bool,
    errors: Vec<String>,
    _subs: Vec<Subscription>,
    _task: Option<Task<()>>,
}

impl EventEmitter<BulkRenameEvent> for BulkRename {}

pub fn new(paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<BulkRename>) -> BulkRename {
    BulkRename::new(paths, window, cx)
}

impl BulkRename {
    pub fn new(paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let dir = paths.first().and_then(|p| p.parent()).map(|p| p.to_path_buf()).unwrap_or_default();
        // Names are known immediately; metadata (dates) and siblings (conflict checks) are read
        // in the background so opening the dialog never touches the disk on the UI thread.
        let names: Vec<String> =
            paths.iter().map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).collect();
        let items: Vec<RenameItem> = names.iter().map(|n| RenameItem::new(n.clone(), false, 0)).collect();
        let siblings: Vec<String> = Vec::new();
        let (paths2, dir2, names2) = (paths.clone(), dir.clone(), names.clone());
        let meta = cx.background_spawn(async move {
            let items: Vec<RenameItem> = paths2
                .iter()
                .zip(&names2)
                .map(|(p, name)| {
                    let (is_dir, ms) = std::fs::metadata(p)
                        .map(|m| {
                            let ms = m
                                .modified()
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_millis() as i64)
                                .unwrap_or(0);
                            (m.is_dir(), ms)
                        })
                        .unwrap_or((false, 0));
                    RenameItem::new(name.clone(), is_dir, ms)
                })
                .collect();
            let batch: HashSet<String> = names2.iter().map(|n| n.to_lowercase()).collect();
            let siblings: Vec<String> = std::fs::read_dir(&dir2)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .filter(|n| !batch.contains(&n.to_lowercase()))
                        .collect()
                })
                .unwrap_or_default();
            (items, siblings)
        });
        cx.spawn(async move |this, cx| {
            let (items, siblings) = meta.await;
            let _ = this.update(cx, |b, cx| {
                b.items = items;
                b.siblings = siblings;
                b.recompute(cx);
            });
        })
        .detach();

        let mk = |window: &mut Window, cx: &mut Context<Self>, ph: &'static str, val: &'static str| {
            cx.new(|cx| InputState::new(window, cx).placeholder(ph).default_value(val))
        };
        let find = mk(window, cx, t!("Aranacak metin"), "");
        let replace = mk(window, cx, t!("Yerine yazılacak"), "");
        let pattern = mk(window, cx, t!("Tatil_{n:03}"), "{name}_{n:02}");
        let start = mk(window, cx, "1", "1");
        let step = mk(window, cx, "1", "1");
        let prefix = mk(window, cx, t!("Önek"), "");
        let suffix = mk(window, cx, t!("Sonek"), "");

        let mut subs = Vec::new();
        for input in [&find, &replace, &pattern, &start, &step, &prefix, &suffix] {
            subs.push(cx.subscribe_in(input, window, |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.recompute(cx);
                }
            }));
        }
        find.update(cx, |s, cx| s.focus(window, cx));

        let mut this = Self {
            dir,
            names,
            items,
            siblings,
            tab: Tab::Find,
            find,
            replace,
            pattern,
            start,
            step,
            prefix,
            suffix,
            case_insensitive: true,
            regex: false,
            include_ext: false,
            case_kind: CaseKind::Lower,
            results: Vec::new(),
            applying: false,
            errors: Vec::new(),
            _subs: subs,
            _task: None,
        };
        this.recompute(cx);
        this
    }

    fn spec(&self, cx: &App) -> RenameSpec {
        let text = |e: &Entity<InputState>| e.read(cx).value().to_string();
        let num = |e: &Entity<InputState>| text(e).trim().parse::<i64>().unwrap_or(1);
        let mode = match self.tab {
            Tab::Find => Mode::FindReplace {
                find: text(&self.find),
                replace: text(&self.replace),
                case_insensitive: self.case_insensitive,
                regex: self.regex,
            },
            Tab::Pattern => Mode::Pattern { pattern: text(&self.pattern), start: num(&self.start), step: num(&self.step) },
            Tab::Case => Mode::Case(self.case_kind),
            Tab::Affix => Mode::PrefixSuffix { prefix: text(&self.prefix), suffix: text(&self.suffix) },
        };
        RenameSpec { mode, include_ext: self.include_ext, existing: self.siblings.clone() }
    }

    fn recompute(&mut self, cx: &mut Context<Self>) {
        let spec = self.spec(cx);
        self.results = rename::compute(&self.items, &spec);
        cx.notify();
    }

    fn conflicts(&self) -> usize {
        self.results.iter().filter(|r| r.is_err()).count()
    }

    fn changed(&self) -> usize {
        self.results.iter().zip(&self.names).filter(|(r, n)| matches!(r, Ok(new) if new != *n)).count()
    }

    fn can_apply(&self) -> bool {
        !self.applying && self.errors.is_empty() && self.conflicts() == 0 && self.changed() > 0
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        if !self.can_apply() {
            return;
        }
        let pairs: Vec<(String, String)> = self
            .names
            .iter()
            .zip(&self.results)
            .filter_map(|(o, r)| r.as_ref().ok().map(|n| (o.clone(), n.clone())))
            .collect();
        let dir = self.dir.clone();
        self.applying = true;
        let work = cx.background_spawn(async move { run_plan(&dir, &pairs) });
        self._task = Some(cx.spawn(async move |this, cx| {
            let errors = work.await;
            let _ = this.update(cx, |v, cx| {
                v.applying = false;
                if errors.is_empty() {
                    cx.emit(BulkRenameEvent::Close);
                } else {
                    v.errors = errors;
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

/// Executes the swap-safe plan; returns one message per failure.
fn run_plan(dir: &std::path::Path, pairs: &[(String, String)]) -> Vec<String> {
    let pid = std::process::id();
    let tmp = |i: usize| {
        let mut n = 0;
        loop {
            let name = format!("~rf{pid}_{i}_{n}.tmp");
            if !dir.join(&name).exists() {
                return name;
            }
            n += 1;
        }
    };
    let temps = std::cell::RefCell::new(std::collections::HashMap::<usize, String>::new());
    let steps = rename::plan(pairs, |i| temps.borrow_mut().entry(i).or_insert_with(|| tmp(i)).clone());
    let temp_names: HashSet<String> = temps.borrow().values().cloned().collect();
    let mut orig_of_tmp = std::collections::HashMap::<String, String>::new();
    for s in &steps {
        if temp_names.contains(&s.to) {
            orig_of_tmp.insert(s.to.clone(), s.from.clone());
        }
    }
    let mut errors = Vec::new();
    let mut dead: HashSet<String> = HashSet::new(); // temp names that never got created
    for s in steps {
        if dead.contains(&s.from) {
            continue;
        }
        if let Err(e) = rf_win::fileops::rename_no_replace(&dir.join(&s.from), &dir.join(&s.to)) {
            let shown = orig_of_tmp.get(&s.from).unwrap_or(&s.from);
            errors.push(format!("{shown} → {}: {e}", s.to));
            if orig_of_tmp.contains_key(&s.to) {
                dead.insert(s.to.clone());
            } else if let Some(orig) = orig_of_tmp.get(&s.from) {
                // moving back in failed: put the file back under its original name
                let _ = std::fs::rename(dir.join(&s.from), dir.join(orig));
            }
        } else if !temp_names.contains(&s.to) {
            let from = orig_of_tmp.get(&s.from).cloned().unwrap_or_else(|| s.from.clone());
            crate::undo::push(crate::undo::UndoOp::Renamed { from: dir.join(from), to: dir.join(&s.to) });
        }
    }
    errors
}

fn checkbox(id: &'static str, label: &'static str, on: bool, p: &Palette) -> Stateful<Div> {
    h_flex()
        .id(id)
        .gap_2()
        .items_center()
        .cursor_pointer()
        .text_sm()
        .child(
            div()
                .size(px(16.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .border_1()
                .border_color(if on { p.accent } else { p.text_tertiary })
                .when(on, |d| d.bg(p.accent))
                .when(on, |d| d.child(Icon::new(L::Check).size(px(12.)).text_color(p.accent_text))),
        )
        .child(label)
}

fn chip(id: impl Into<ElementId>, label: &'static str, on: bool, p: &Palette) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded(theme::RADIUS_SM)
        .text_sm()
        .cursor_pointer()
        .when(on, |d| d.bg(p.card_bg).text_color(p.text).shadow_sm())
        .when(!on, |d| d.text_color(p.text_secondary).hover(|s| s.text_color(p.text)))
        .child(label)
}

fn button(id: &'static str, label: &'static str, primary: bool, enabled: bool, p: &Palette) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(32.))
        .px_4()
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::RADIUS_SM)
        .text_sm()
        .when(primary, |d| d.bg(p.accent).text_color(p.accent_text))
        .when(!primary, |d| d.bg(p.hover))
        .when(enabled, |d| d.cursor_pointer().hover(|s| s.opacity(0.85)))
        .when(!enabled, |d| d.opacity(0.45))
        .child(label)
}

fn field(label: &'static str, input: &Entity<InputState>, p: &Palette) -> Div {
    v_flex()
        .gap_1()
        .flex_1()
        .min_w_0()
        .child(div().text_xs().text_color(p.text_secondary).child(label))
        .child(Input::new(input))
}

impl Render for BulkRename {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let conflicts = self.conflicts();
        let changed = self.changed();
        let can_apply = self.can_apply();
        let has_errors = !self.errors.is_empty();

        let header = h_flex()
            .px_4()
            .py_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(p.divider)
            .child(Icon::new(L::List).size(px(18.)).text_color(p.text_secondary))
            .child(
                v_flex()
                    .flex_1()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(t!("Toplu yeniden adlandır")))
                    .child(div().text_xs().text_color(p.text_secondary).child(crate::i18n::items(self.names.len()))),
            )
            .child(
                div()
                    .id("br-close")
                    .size(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::RADIUS_SM)
                    .cursor_pointer()
                    .hover(|s| s.bg(p.hover))
                    .child(Icon::new(L::X).size(px(14.)))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(BulkRenameEvent::Close))),
            );

        // Mode segmented control
        let tabs = [(Tab::Find, t!("Bul ve değiştir")), (Tab::Pattern, t!("Desen")), (Tab::Case, t!("Büyük/küçük harf")), (Tab::Affix, t!("Önek/sonek"))];
        let mut seg = h_flex().p_0p5().gap_0p5().rounded(theme::RADIUS).bg(p.hover);
        for (i, (t, label)) in tabs.into_iter().enumerate() {
            seg = seg.child(chip(("br-tab", i), label, self.tab == t, &p).on_click(cx.listener(move |this, _, _, cx| {
                this.tab = t;
                this.recompute(cx);
            })));
        }

        let controls: AnyElement = match self.tab {
            Tab::Find => v_flex()
                .gap_2()
                .child(h_flex().gap_3().child(field(t!("Bul"), &self.find, &p)).child(field(t!("Değiştir"), &self.replace, &p)))
                .child(
                    h_flex()
                        .gap_4()
                        .child(checkbox("br-ci", t!("Büyük/küçük harfe duyarsız"), self.case_insensitive, &p).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.case_insensitive = !this.case_insensitive;
                                this.recompute(cx);
                            }),
                        ))
                        .child(checkbox("br-re", t!("Düzenli ifade (regex, $1 grupları)"), self.regex, &p).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.regex = !this.regex;
                                this.recompute(cx);
                            },
                        ))),
                )
                .into_any_element(),
            Tab::Pattern => v_flex()
                .gap_2()
                .child(
                    h_flex()
                        .gap_3()
                        .child(field(t!("Desen"), &self.pattern, &p))
                        .child(div().w(px(80.)).child(field(t!("Başlangıç"), &self.start, &p)))
                        .child(div().w(px(80.)).child(field(t!("Adım"), &self.step, &p))),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(p.text_secondary)
                        .child(t!("{n} sayaç ({n:03} sıfırla doldurur) · {name} özgün ad · {ext} uzantı · {date} değiştirme tarihi (YYYY-AA-GG)")),
                )
                .into_any_element(),
            Tab::Case => {
                let mut row = h_flex().p_0p5().gap_0p5().rounded(theme::RADIUS).bg(p.hover).w_auto();
                for (i, (k, label)) in [(CaseKind::Lower, t!("küçük harf")), (CaseKind::Upper, t!("BÜYÜK HARF")), (CaseKind::Title, t!("Başlık Düzeni"))]
                    .into_iter()
                    .enumerate()
                {
                    row = row.child(chip(("br-case", i), label, self.case_kind == k, &p).on_click(cx.listener(move |this, _, _, cx| {
                        this.case_kind = k;
                        this.recompute(cx);
                    })));
                }
                h_flex().child(row).into_any_element()
            }
            Tab::Affix => h_flex()
                .gap_3()
                .child(field(t!("Önek"), &self.prefix, &p))
                .child(field(t!("Sonek (uzantıdan önce)"), &self.suffix, &p))
                .into_any_element(),
        };

        let options = h_flex().items_center().child(
            checkbox("br-ext", t!("Uzantıyı da değiştir"), self.include_ext, &p).on_click(cx.listener(|this, _, _, cx| {
                this.include_ext = !this.include_ext;
                this.recompute(cx);
            })),
        );

        let top = v_flex().px_4().py_3().gap_3().child(h_flex().child(seg)).child(controls).child(options);

        let list_head = h_flex()
            .px_4()
            .py_1()
            .gap_2()
            .text_xs()
            .text_color(p.text_secondary)
            .border_t_1()
            .border_b_1()
            .border_color(p.divider)
            .child(div().flex_1().child(t!("Eski ad")))
            .child(div().w(px(16.)))
            .child(div().flex_1().child(t!("Yeni ad")));

        let n = self.names.len();
        let list = uniform_list(
            "br-rows",
            n,
            cx.processor(move |this, range: std::ops::Range<usize>, window, _cx| {
                let p = Palette::for_window(window);
                range
                    .map(|i| {
                        let old = this.names[i].clone();
                        let (new_text, err, changed) = match &this.results[i] {
                            Ok(nn) => (nn.clone(), None, *nn != old),
                            Err(e) => (String::new(), Some(crate::i18n::td(&e.to_string())), false),
                        };
                        let bad = err.is_some();
                        h_flex()
                            .id(("br-row", i))
                            .w_full()
                            .h(px(28.))
                            .px_3()
                            .gap_2()
                            .items_center()
                            .rounded(theme::RADIUS_SM)
                            .text_sm()
                            .when(bad, |d| d.bg(p.danger.opacity(0.10)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_color(p.text_secondary)
                                    .child(old),
                            )
                            .child(Icon::new(L::ArrowRight).size(px(14.)).text_color(p.text_tertiary))
                            .child(match err {
                                Some(e) => h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .items_center()
                                    .text_color(p.danger)
                                    .child(Icon::new(L::TriangleAlert).size(px(13.)))
                                    .child(div().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(e))
                                    .into_any_element(),
                                None => div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .when(changed, |d| d.text_color(p.text).font_weight(FontWeight::MEDIUM))
                                    .when(!changed, |d| d.text_color(p.text_tertiary))
                                    .child(new_text)
                                    .into_any_element(),
                            })
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .p_1p5();

        let status = if has_errors {
            format!("{} {}", crate::i18n::items(self.errors.len()), t!("yeniden adlandırılamadı"))
        } else if self.applying {
            t!("Uygulanıyor…").to_string()
        } else if conflicts > 0 {
            format!("{conflicts} {}", t!("çakışma var, düzeltmeden uygulanamaz"))
        } else {
            format!("{} {}", crate::i18n::items(changed), t!("değişecek"))
        };

        let footer = h_flex()
            .px_4()
            .py_3()
            .gap_2()
            .items_center()
            .border_t_1()
            .border_color(p.divider)
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .text_color(if conflicts > 0 || has_errors { p.danger } else { p.text_secondary })
                    .child(status),
            )
            .child(
                button("br-cancel", if has_errors { t!("Kapat") } else { t!("İptal") }, false, true, &p)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(BulkRenameEvent::Close))),
            )
            .child(button("br-apply", t!("Uygula"), true, can_apply, &p).on_click(cx.listener(|this, _, _, cx| this.apply(cx))));

        let errors_box = has_errors.then(|| {
            v_flex()
                .max_h(px(90.))
                .overflow_hidden()
                .px_4()
                .py_2()
                .gap_0p5()
                .bg(p.danger.opacity(0.10))
                .text_xs()
                .text_color(p.danger)
                .children(self.errors.iter().take(6).map(|e| div().child(crate::i18n::td(e))))
        });

        v_flex()
            .w(px(720.))
            .h(px(520.))
            .rounded(px(12.))
            .overflow_hidden()
            .bg(p.card_bg)
            .border_1()
            .border_color(p.border)
            .shadow_lg()
            .text_color(p.text)
            .child(header)
            .child(top)
            .child(list_head)
            .child(list)
            .children(errors_box)
            .child(footer)
    }
}
