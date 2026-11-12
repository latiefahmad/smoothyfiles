//! Settings page (Location::Settings), laid out like Files: a navigation column on the left,
//! one page of cards on the right. Every card is one setting: icon, title, optional
//! description, and its control (a switch, a drop-down that opens our Acrylic menu, buttons) —
//! or an expander with child settings.

use std::collections::HashSet;

use gpui_kit::assets::IconName as L;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_windows::hybrid;

use crate::fluent_menu::{self, Item, Menu, MenuIcon};
use crate::select_box::{Choice, select_box};
use crate::i18n::Lang;
use crate::settings::{
    self, AnimPref, BackdropPref, ClickPref, DatePref, DefaultView, DeletePref, IdePref, RendererPref, Settings, StartupPref, ThemePref,
};
use crate::t;
use crate::look::config::LookSettings;
use crate::theme::{self, Palette};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    General,
    Appearance,
    Layout,
    Files,
    Actions,
    Tags,
    Look,
    Dev,
    Advanced,
    Speed,
    About,
}

const PAGES: [(Page, L, &str); 11] = [
    (Page::General, L::Settings, "Genel"),
    (Page::Appearance, L::Palette, "Görünüm"),
    (Page::Layout, L::LayoutGrid, "Sayfa düzeni"),
    (Page::Files, L::Folder, "Dosyalar ve klasörler"),
    (Page::Actions, L::Keyboard, "Eylemler"),
    (Page::Tags, L::Tag, "Etiketler"),
    (Page::Look, L::Eye, "DirectLook"),
    (Page::Dev, L::SquareTerminal, "Geliştirici araçları"),
    (Page::Advanced, L::Wrench, "Gelişmiş"),
    (Page::Speed, L::Gauge, "Hız testi"),
    (Page::About, L::Info, "Hakkında"),
];

/// Which page is shown and which expanders are open (kept while the app runs).
struct Ui {
    page: Page,
    open: HashSet<&'static str>,
}
impl Global for Ui {}

fn ui(cx: &mut App) -> &mut Ui {
    if !cx.has_global::<Ui>() {
        cx.set_global(Ui { page: Page::General, open: HashSet::new() });
    }
    cx.global_mut::<Ui>()
}

pub fn open_page(page: Page, cx: &mut App) {
    ui(cx).page = page;
}

// ---------------------------------------------------------------- controls

/// A Windows 11 switch with its state word ("Açık"/"Kapalı") on the left.
pub(crate) fn switch(id: impl Into<ElementId>, p: &Palette, on: bool, toggle: impl Fn(&mut Window, &mut App) + 'static) -> AnyElement {
    let knob = if on { p.accent_text } else { p.text_secondary };
    h_flex()
        .gap_3()
        .items_center()
        .child(div().text_sm().text_color(p.text_secondary).child(if on { t!("Açık") } else { t!("Kapalı") }))
        .child(
            h_flex()
                .id(id)
                .w(px(40.))
                .h(px(20.))
                .px(px(3.))
                .items_center()
                .rounded_full()
                .border_1()
                .when(on, |d| d.bg(p.accent).border_color(p.accent).justify_end())
                .when(!on, |d| d.border_color(p.text_secondary).hover(|s| s.bg(p.hover)))
                .child(div().size(px(12.)).rounded_full().bg(knob))
                .on_click(move |_, window, cx| toggle(window, cx)),
        )
        .into_any_element()
}

/// Switch bound to a boolean setting.
fn sw(id: &'static str, p: &Palette, on: bool, set: fn(&mut Settings, bool)) -> AnyElement {
    switch(id, p, on, move |_, cx| settings::update(cx, |s| set(s, !on)))
}

/// A button showing the current value; clicking opens the choices in our menu.
fn dropdown<T: Copy + PartialEq + 'static>(id: &'static str, p: &Palette, current: T, options: Vec<(T, String)>, set: fn(&mut Settings, T)) -> AnyElement {
    let label = options.iter().find(|(v, _)| *v == current).map(|(_, l)| l.clone()).unwrap_or_default();
    select_box(id, p, label, move |_| {
        options.iter().map(|(v, l)| {
            let v = *v;
            Choice::new(l.clone(), v == current, move |_, cx| settings::update(cx, |s| set(s, v)))
        }).collect()
    })
}

fn button(id: impl Into<ElementId>, p: &Palette, label: impl Into<SharedString>, run: impl Fn(&mut Window, &mut App) + 'static) -> AnyElement {
    div()
        .id(id)
        .h(px(32.))
        .px_4()
        .flex()
        .items_center()
        .rounded(theme::RADIUS_SM)
        .bg(p.hover)
        .border_1()
        .border_color(p.border)
        .text_sm()
        .hover(|s| s.bg(p.pressed))
        .active(|s| s.opacity(0.8))
        .child(label.into())
        .on_click(move |_, window, cx| run(window, cx))
        .into_any_element()
}

// ---------------------------------------------------------------- layout pieces

fn card_body(p: &Palette, icon: L, title: SharedString, desc: Option<SharedString>, control: Option<AnyElement>) -> Div {
    h_flex()
        .min_h(px(64.))
        .px_4()
        .py_3()
        .gap_4()
        .items_center()
        .child(Icon::new(icon).size(px(18.)).flex_shrink_0().text_color(p.text))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().child(title))
                .children(desc.map(|d| div().text_xs().text_color(p.text_secondary).child(d))),
        )
        .children(control)
}

fn shell(p: &Palette) -> Div {
    v_flex().w_full().rounded(theme::RADIUS).bg(p.hover).border_1().border_color(p.border)
}

/// One setting row.
fn card(p: &Palette, icon: L, title: &str, desc: Option<&str>, control: AnyElement) -> AnyElement {
    shell(p)
        .child(card_body(p, icon, title.to_string().into(), desc.map(|d| d.to_string().into()), Some(control)))
        .into_any_element()
}

/// A card that opens to show `children` (state kept per `key`).
fn expander(key: &'static str, p: &Palette, icon: L, title: &str, desc: Option<&str>, control: Option<AnyElement>, children: impl FnOnce() -> Vec<AnyElement>, cx: &mut App) -> AnyElement {
    let open = ui(cx).open.contains(key);
    let chevron = div()
        .id(SharedString::from(format!("exp-chev-{key}")))
        .size(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::RADIUS_SM)
        .hover(|s| s.bg(p.pressed))
        .child(Icon::new(if open { L::ChevronUp } else { L::ChevronDown }).size(px(14.)))
        .on_click(move |_, window, cx| {
            let u = ui(cx);
            if !u.open.remove(key) {
                u.open.insert(key);
            }
            window.refresh();
        });
    let header_control = h_flex().gap_2().items_center().children(control).child(chevron).into_any_element();
    shell(p)
        .child(card_body(p, icon, title.to_string().into(), desc.map(|d| d.to_string().into()), Some(header_control)))
        .when(open, |d| {
            d.child(div().h(px(1.)).bg(p.divider)).child(v_flex().py_1().children(children()))
        })
        .into_any_element()
}

/// A child row inside an expander (no icon, indented like Files).
fn sub(p: &Palette, title: &str, desc: Option<&str>, control: AnyElement) -> AnyElement {
    h_flex()
        .min_h(px(48.))
        .pl(px(56.))
        .pr_4()
        .py_2()
        .gap_4()
        .items_center()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(div().text_sm().child(title.to_string()))
                .children(desc.map(|d| div().text_xs().text_color(p.text_secondary).child(d.to_string()))),
        )
        .child(control)
        .into_any_element()
}

fn section(p: &Palette, title: &str) -> AnyElement {
    div().pt_5().pb_1().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.text).child(title.to_string()).into_any_element()
}

fn link_row(p: &Palette, id: &'static str, icon: L, title: &str, desc: Option<&str>, run: impl Fn(&mut Window, &mut App) + 'static) -> AnyElement {
    shell(p)
        .id(id)
        .hover(|s| s.bg(p.pressed))
        .on_click(move |_, window, cx| run(window, cx))
        .child(card_body(
            p,
            icon,
            title.to_string().into(),
            desc.map(|d| d.to_string().into()),
            Some(Icon::new(L::SquareArrowOutUpRight).size(px(14.)).text_color(p.text_secondary).into_any_element()),
        ))
        .into_any_element()
}

// ---------------------------------------------------------------- page

pub fn render(p: &Palette, cx: &mut App) -> AnyElement {
    let page = ui(cx).page;
    let s = cx.global::<Settings>().clone();
    let mut nav = v_flex().w(px(232.)).flex_shrink_0().h_full().px_3().pt_5().gap_0p5().child(
        div().px_2().pb_3().text_xl().font_weight(FontWeight::SEMIBOLD).child(t!("Ayarlar")),
    );
    for (i, (pg, icon, label)) in PAGES.iter().copied().enumerate() {
        let on = pg == page;
        nav = nav.child(
            h_flex()
                .id(("set-nav", i))
                .h(px(36.))
                .px_3()
                .gap_3()
                .items_center()
                .relative()
                .rounded(theme::RADIUS_SM)
                .text_sm()
                .when(on, |d| d.bg(p.selection).child(theme::selection_pill(p)))
                .when(!on, |d| d.hover(|s| s.bg(p.hover)))
                .child(Icon::new(icon).size(px(16.)).text_color(if on { p.accent } else { p.text_secondary }))
                .child(crate::i18n::t(label))
                .on_click(move |_, window, cx| {
                    open_page(pg, cx);
                    window.refresh();
                }),
        );
    }
    let title = PAGES.iter().find(|x| x.0 == page).map(|x| crate::i18n::t(x.2)).unwrap_or_default();
    let body: Vec<AnyElement> = match page {
        Page::General => general(p, &s, cx),
        Page::Appearance => appearance(p, &s, cx),
        Page::Layout => layout(p, &s, cx),
        Page::Files => files(p, &s, cx),
        Page::Actions => actions(p, cx),
        Page::Tags => tags(p, cx),
        Page::Look => look(p, &s, cx),
        Page::Dev => dev(p, &s),
        Page::Advanced => advanced(p, &s),
        Page::Speed => crate::bench_view::page(p, cx),
        Page::About => about(p, cx),
    };
    h_flex()
        .size_full()
        .child(nav)
        .child(
            div().id("settings-page").flex_1().min_w_0().h_full().overflow_y_scroll().child(
                v_flex()
                    .w_full()
                    .max_w(px(980.))
                    .px_6()
                    .pt_5()
                    .pb_8()
                    .gap_2()
                    .child(div().pb_2().text_xl().font_weight(FontWeight::SEMIBOLD).child(title))
                    .children(body),
            ),
        )
        .into_any_element()
}

/// Quick search: on/off, its shortcut (recorded like the keyboard shortcuts), web engine,
/// bookmarks.
fn quick_search_cards(p: &Palette, s: &Settings, cx: &mut App) -> Vec<AnyElement> {
    let capture = crate::quick_search::CAPTURE_ACTION;
    let waiting = crate::keymap_editor::capturing(cx).as_deref() == Some(capture);
    let shown: SharedString = if waiting { t!("Tuşlara basın…").into() } else { crate::keymap_editor::pretty(&s.quick_search_keys).into() };
    let keys_desc = match crate::quick_search::problem() {
        Some(rf_win::hotkey::HotkeyError::Taken) if s.quick_search => t!("Bu kısayolu başka bir uygulama kullanıyor (ChatGPT, Gemini, PowerToys…): başka bir kısayol seçin"),
        Some(rf_win::hotkey::HotkeyError::Invalid) if s.quick_search => t!("Bu kısayol kullanılamıyor: Ctrl, Alt, Shift ya da Win ile bir tuş seçin"),
        _ => t!("Değiştirmek için tıklayıp yeni tuşlara basın"),
    };
    let warn = s.quick_search && crate::quick_search::problem().is_some();
    let pill = div()
        .id("qs-keys")
        .px_3()
        .py_1()
        .rounded(px(4.))
        .border_1()
        .border_color(if warn { p.danger } else { p.border })
        .text_sm()
        .cursor_pointer()
        .text_color(if waiting { p.accent } else { p.text })
        .hover(|d| d.bg(p.pressed))
        .when(waiting, |d| d.bg(p.selection))
        .child(shown)
        .on_click(move |_, window, cx| {
            crate::keymap_editor::start(capture.to_string(), cx);
            window.refresh();
        })
        .into_any_element();
    vec![
        section(p, t!("Hızlı arama")),
        card(
            p,
            L::Search,
            t!("Hızlı arama"),
            Some(if s.run_in_background {
                t!("Her yerden kısayolla: uygulamalar, dosyalar, klasörler, yer imleri ve web")
            } else {
                t!("Her yerden kısayolla: uygulamalar, dosyalar, klasörler, yer imleri ve web. Pencere kapalıyken de çalışması için Gelişmiş'te arka planda çalışmayı açın")
            }),
            sw("quick-search", p, s.quick_search, |s, v| s.quick_search = v),
        ),
        card(p, L::Keyboard, t!("Kısayol"), Some(keys_desc), pill),
        card(
            p,
            L::Globe,
            t!("Web araması"),
            None,
            dropdown(
                "web-search",
                p,
                s.web_search,
                vec![
                    (crate::settings::WebSearch::Google, "Google".into()),
                    (crate::settings::WebSearch::Bing, "Bing".into()),
                    (crate::settings::WebSearch::DuckDuckGo, "DuckDuckGo".into()),
                    (crate::settings::WebSearch::Yandex, "Yandex".into()),
                ],
                |s, v| s.web_search = v,
            ),
        ),
        card(
            p,
            L::Bookmark,
            t!("Chrome ve Edge yer imlerini de ara"),
            Some(t!("Yer imleri bu bilgisayarda okunur, hiçbir yere gönderilmez")),
            sw("qs-bookmarks", p, s.quick_search_bookmarks, |s, v| s.quick_search_bookmarks = v),
        ),
    ]
}

fn general(p: &Palette, s: &Settings, cx: &mut App) -> Vec<AnyElement> {
    let mut v = vec![
        card(
            p,
            L::Languages,
            t!("Dil"),
            None,
            dropdown(
                "lang",
                p,
                s.language,
                std::iter::once((Lang::System, t!("Sistem ayarını kullan").to_string()))
                    .chain(Lang::all().into_iter().map(|l| (l, l.native_name().to_string())))
                    .collect(),
                |s, v| s.language = v,
            ),
        ),
        card(
            p,
            L::CalendarClock,
            t!("Tarih biçimi"),
            Some(t!("Uygulama: \"5 dakika önce\"; Sistem: 05.11.2026 13:45; Evrensel: 2026-11-05 13:45")),
            dropdown(
                "datefmt",
                p,
                s.date_format,
                vec![
                    (DatePref::Application, t!("Uygulama").into()),
                    (DatePref::System, t!("Sistem").into()),
                    (DatePref::Universal, t!("Evrensel").into()),
                ],
                |s, v| s.date_format = v,
            ),
        ),
    ];
    let startup_dd = dropdown(
        "startup",
        p,
        s.startup,
        vec![
            (StartupPref::Continue, t!("Kaldığınız yerden devam edin").into()),
            (StartupPref::NewTab, t!("Yeni sekme aç").into()),
            (StartupPref::Pages, t!("Belirli sayfaları aç").into()),
        ],
        |s, v| s.startup = v,
    );
    let pages = s.startup_pages.clone();
    let pp = *p;
    v.push(expander(
        "startup",
        p,
        L::Power,
        t!("Başlangıç ayarları"),
        None,
        Some(startup_dd),
        move || {
            let mut rows: Vec<AnyElement> = pages
                .iter()
                .enumerate()
                .map(|(i, page)| {
                    let shown = if page.is_empty() { t!("Ana Sayfa").to_string() } else { page.clone() };
                    sub(
                        &pp,
                        &shown,
                        None,
                        button(("sp-del", i), &pp, t!("Kaldır"), move |_, cx| settings::update(cx, |s| {
                            if i < s.startup_pages.len() {
                                s.startup_pages.remove(i);
                            }
                        })),
                    )
                })
                .collect();
            rows.push(sub(
                &pp,
                t!("Belirli sayfalar"),
                Some(t!("\"Belirli sayfaları aç\" seçiliyken açılır.")),
                button("sp-use", &pp, t!("Şu an açık sekmeleri kullan"), |_, cx| {
                    settings::update(cx, |s| {
                        s.startup_pages = s.session_tabs.clone();
                        s.startup = StartupPref::Pages;
                    })
                }),
            ));
            rows
        },
        cx,
    ));
    v.push(card(p, L::SquarePlus, t!("Her zaman yeni açılan sekmeye geç"), None, sw("focus-tab", p, s.focus_new_tab, |s, v| s.focus_new_tab = v)));

    v.push(section(p, t!("Çift bölme")));
    v.push(card(p, L::Columns2, t!("Yeni sekmeleri çift bölmeyle aç"), None, sw("dual-new", p, s.dual_pane_new_tabs, |s, v| s.dual_pane_new_tabs = v)));
    v.push(card(
        p,
        L::SquareSplitHorizontal,
        t!("Çift bölme yönü"),
        None,
        dropdown(
            "dual-dir",
            p,
            s.dual_pane_vertical,
            vec![(true, t!("Yan yana").into()), (false, t!("Alt alta").into())],
            |s, v| s.dual_pane_vertical = v,
        ),
    ));

    v.push(section(p, t!("Sağ tık menüsü")));
    let m = s.menu.clone();
    let pp = *p;
    v.push(expander(
        "ctx",
        p,
        L::SquareMenu,
        t!("Bağlam menüsü seçenekleri"),
        Some(t!("Menüde hangi komutların görüneceği")),
        None,
        move || {
            type Set = fn(&mut Settings, bool);
            let rows: [(&str, bool, Set); 9] = [
                (t!("Yeni sekmede aç"), m.open_in_new_tab, |s, v| s.menu.open_in_new_tab = v),
                (t!("Yeni pencerede aç"), m.open_in_new_window, |s, v| s.menu.open_in_new_window = v),
                (t!("Öğe yolunu kopyala"), m.copy_path, |s, v| s.menu.copy_path = v),
                (t!("Kısayol oluştur"), m.create_shortcut, |s, v| s.menu.create_shortcut = v),
                (t!("Kenar çubuğuna sabitle"), m.pin, |s, v| s.menu.pin = v),
                (t!("Sıkıştır / Çıkar"), m.compress, |s, v| s.menu.compress = v),
                (t!("Etiketler"), m.tags, |s, v| s.menu.tags = v),
                (t!("Rafa ekle"), m.shelf, |s, v| s.menu.shelf = v),
                (t!("Terminalde aç"), m.terminal, |s, v| s.menu.terminal = v),
            ];
            rows.into_iter()
                .enumerate()
                .map(|(i, (label, on, set))| sub(&pp, label, None, switch(("ctx-sw", i), &pp, on, move |_, cx| settings::update(cx, |s| set(s, !on)))))
                .collect()
        },
        cx,
    ));
    v.push(card(
        p,
        L::Ellipsis,
        t!("Kabuk uzantılarını alt menüye taşı"),
        Some(t!("Kapalıyken diğer uygulamaların öğeleri menünün sonunda listelenir.")),
        sw("shell-sub", p, s.menu.shell_submenu, |s, v| s.menu.shell_submenu = v),
    ));

    v.push(section(p, t!("Kaydırma")));
    v.push(card(p, L::ChevronsUpDown, t!("Akıcı kaydırma"), None, sw("smooth", p, s.smooth_scroll, |s, v| s.smooth_scroll = v)));
    v.extend(quick_search_cards(p, s, cx));
    v
}

/// Background tint presets (Files' "Background color").
const TINTS: [(&str, u32); 15] = [
    ("Altın sarısı", 0xC19C00),
    ("Turuncu", 0xCA5010),
    ("Tuğla kırmızısı", 0xA4262C),
    ("Kırmızı", 0xD13438),
    ("Gül", 0xE3008C),
    ("Mor", 0x881798),
    ("İris", 0x8764B8),
    ("Mavi", 0x0078D4),
    ("Deniz", 0x038387),
    ("Nane", 0x00B294),
    ("Yeşil", 0x107C10),
    ("Kamuflaj", 0x486860),
    ("Fırtına", 0x4C4A48),
    ("Mavi gri", 0x515C6B),
    ("Gri", 0x7A7574),
];

fn appearance(p: &Palette, s: &Settings, cx: &mut App) -> Vec<AnyElement> {
    let mut v = vec![
        card(
            p,
            L::SunMoon,
            t!("Tema"),
            None,
            dropdown(
                "theme",
                p,
                s.theme,
                vec![(ThemePref::System, t!("Sistem ayarını kullan").into()), (ThemePref::Light, crate::i18n::tx("Açık", "Light").into()), (ThemePref::Dark, t!("Koyu").into())],
                |s, v| s.theme = v,
            ),
        ),
        card(
            p,
            L::Layers,
            t!("Arka plan malzemesi"),
            Some(t!("Mica masaüstü renginden ton alır; Mica Alt daha renklidir.")),
            dropdown(
                "backdrop",
                p,
                s.backdrop,
                vec![
                    (BackdropPref::Mica, "Mica".into()),
                    (BackdropPref::MicaAlt, "Mica Alt".into()),
                    (BackdropPref::Acrylic, t!("Akrilik").into()),
                    (BackdropPref::Solid, t!("Düz").into()),
                ],
                |s, v| s.backdrop = v,
            ),
        ),
        card(
            p,
            L::Activity,
            t!("Animasyonlar"),
            Some(t!("Sekme açılırken ve kapanırken kısa geçişler. Hiçbir işi geciktirmez.")),
            dropdown(
                "animations",
                p,
                s.animations,
                vec![
                    (AnimPref::System, t!("Windows ayarına uy").into()),
                    (AnimPref::On, t!("Açık").into()),
                    (AnimPref::Off, t!("Kapalı").into()),
                ],
                |s, v| s.animations = v,
            ),
        ),
        card(
            p,
            L::PanelRight,
            t!("Kaydırma çubuğunu göster"),
            Some(t!("Ayrıntılar ve Izgara listelerinin sağında; sürükleyin ya da bir yere tıklayıp oraya atlayın.")),
            sw("scrollbar", p, s.show_scrollbar, |s, v| s.show_scrollbar = v),
        ),
    ];
    // tint swatches
    let tint = s.tint;
    let pp = *p;
    let swatch_now: AnyElement = match tint {
        Some(c) => div().size(px(20.)).rounded(px(4.)).bg(rgb(c)).border_1().border_color(p.border).into_any_element(),
        None => div().text_sm().text_color(p.text_secondary).child(t!("Varsayılan")).into_any_element(),
    };
    v.push(expander(
        "tint",
        p,
        L::Paintbrush,
        t!("Arka plan rengi"),
        None,
        Some(swatch_now),
        move || {
            let tile = |id: usize, label: String, color: Option<u32>, selected: bool| {
                v_flex()
                    .id(("tint", id))
                    .w(px(96.))
                    .gap_1()
                    .items_center()
                    .p_1()
                    .rounded(theme::RADIUS_SM)
                    .border_1()
                    .border_color(if selected { pp.accent } else { transparent_black() })
                    .hover(|s| s.bg(pp.pressed))
                    .child(
                        div()
                            .w_full()
                            .h(px(44.))
                            .rounded(theme::RADIUS_SM)
                            .border_1()
                            .border_color(pp.border)
                            .when_some(color, |d, c| d.bg(hsla_alpha(c, 0.55)))
                            .when(color.is_none(), |d| d.bg(pp.content_bg)),
                    )
                    .child(div().text_xs().text_color(pp.text_secondary).child(label))
                    .on_click(move |_, _, cx| settings::update(cx, |s| s.tint = color))
            };
            let mut grid = div().px_4().py_3().flex().flex_wrap().gap_2();
            grid = grid.child(tile(0, t!("Varsayılan").to_string(), None, tint.is_none()));
            for (i, (name, c)) in TINTS.iter().enumerate() {
                grid = grid.child(tile(i + 1, crate::i18n::t(name).to_string(), Some(*c), tint == Some(*c)));
            }
            vec![grid.into_any_element()]
        },
        cx,
    ));
    // background image
    let img = s.background_image.clone();
    let opacity = s.background_opacity;
    let pp = *p;
    let img_desc = img.as_ref().map(|i| i.display().to_string());
    v.push(expander(
        "bgimg",
        p,
        L::Image,
        t!("Arka plan resmi"),
        Some(img_desc.as_deref().unwrap_or(t!("Yok"))),
        Some(button("bg-browse", p, t!("Gözat"), |window, cx| pick_into(window, cx, t!("Arka plan resmi"), PickTarget::BackgroundImage))),
        move || {
            vec![
                sub(
                    &pp,
                    t!("Görünürlük"),
                    None,
                    dropdown(
                        "bg-opacity",
                        &pp,
                        (opacity * 100.).round() as u32,
                        vec![(10, "%10".into()), (20, "%20".into()), (35, "%35".into()), (50, "%50".into())],
                        |s, v| s.background_opacity = v as f32 / 100.,
                    ),
                ),
                sub(&pp, t!("Resmi kaldır"), None, button("bg-clear", &pp, t!("Kaldır"), |_, cx| settings::update(cx, |s| s.background_image = None))),
            ]
        },
        cx,
    ));
    // font
    let fonts = font_choices(cx);
    let current_font = s.font.clone().unwrap_or_default();
    let mut font_opts: Vec<(usize, String)> = vec![(0, t!("Varsayılan").to_string())];
    font_opts.extend(fonts.iter().enumerate().map(|(i, f)| (i + 1, f.clone())));
    let cur_ix = fonts.iter().position(|f| *f == current_font).map(|i| i + 1).unwrap_or(0);
    let fonts2 = fonts.clone();
    v.push(card(
        p,
        L::Type,
        t!("Yazı tipi"),
        None,
        select_box("font", p, font_opts.iter().find(|(i, _)| *i == cur_ix).map(|x| x.1.clone()).unwrap_or_default(), move |_| {
            font_opts
                .iter()
                .map(|(i, l)| {
                    let name = if *i == 0 { None } else { fonts2.get(i - 1).cloned() };
                    Choice::new(l.clone(), *i == cur_ix, move |_, cx| {
                        let n = name.clone();
                        settings::update(cx, |s| s.font = n)
                    })
                })
                .collect()
        }),
    ));
    v.push(card(p, L::PanelTop, t!("Araç çubuğunu göster"), Some("Ctrl+Shift+B"), sw("toolbar", p, s.show_toolbar, |s, v| s.show_toolbar = v)));
    v.push(card(p, L::PanelBottom, t!("Durum çubuğunu göster"), None, sw("statusbar", p, s.show_statusbar, |s, v| s.show_statusbar = v)));
    v.push(card(p, L::PanelRight, t!("Önizleme paneli"), Some("Alt+P"), sw("preview", p, s.preview_open, |s, v| s.preview_open = v)));
    let hidden = s.sidebar_hidden.clone();
    let libs = s.show_libraries;
    let pp = *p;
    v.push(expander(
        "sidebar",
        p,
        L::PanelLeft,
        t!("Kenar çubuğu bölümleri"),
        None,
        None,
        move || {
            let shown = |k: &str| !hidden.iter().any(|h| h == k);
            vec![
                sub(&pp, t!("Sürücüler"), None, switch("sb-drives", &pp, shown("drives"), {
                    let on = shown("drives");
                    move |_, cx| settings::update(cx, |s| show_section(s, "drives", !on))
                })),
                sub(&pp, t!("Bulut ve Linux"), None, switch("sb-cloud", &pp, shown("cloud"), {
                    let on = shown("cloud");
                    move |_, cx| settings::update(cx, |s| show_section(s, "cloud", !on))
                })),
                sub(&pp, t!("Kütüphaneler"), Some(t!("Sabitlenenlerle aynı klasörleri gösterir")), switch("sb-libs", &pp, libs, move |_, cx| {
                    settings::update(cx, |s| s.show_libraries = !libs)
                })),
                sub(&pp, t!("Ağ"), None, switch("sb-net", &pp, shown("network"), {
                    let on = shown("network");
                    move |_, cx| settings::update(cx, |s| show_section(s, "network", !on))
                })),
                sub(&pp, t!("Etiketler"), None, switch("sb-tags", &pp, shown("tags"), {
                    let on = shown("tags");
                    move |_, cx| settings::update(cx, |s| show_section(s, "tags", !on))
                })),
            ]
        },
        cx,
    ));
    v
}

fn hsla_alpha(c: u32, a: f32) -> Hsla {
    let mut h: Hsla = rgb(c).into();
    h.a = a;
    h
}

/// Installed UI-friendly fonts (curated, only those present).
fn font_choices(cx: &App) -> Vec<String> {
    let all: HashSet<String> = cx.text_system().all_font_names().into_iter().collect();
    [
        "Segoe UI Variable Text",
        "Segoe UI",
        "Inter",
        "Aptos",
        "Calibri",
        "Arial",
        "Verdana",
        "Tahoma",
        "Roboto",
        "Open Sans",
        "Noto Sans",
        "Source Sans 3",
        "Cascadia Code",
        "Consolas",
    ]
    .iter()
    .filter(|f| all.contains(**f))
    .map(|f| f.to_string())
    .collect()
}

fn layout(p: &Palette, s: &Settings, cx: &mut App) -> Vec<AnyElement> {
    use rf_core::SortKey;
    let mut v = vec![
        card(
            p,
            L::FolderSync,
            t!("Düzen ve sıralamayı tüm klasörlerde eşitle"),
            Some(t!("Kapalıyken her klasör kendi görünümünü ve sıralamasını hatırlar.")),
            sw("sync-layout", p, s.sync_layout, |s, v| s.sync_layout = v),
        ),
        card(
            p,
            L::LayoutGrid,
            t!("Varsayılan düzen"),
            Some(t!("Uyarlanabilir: çoğunlukla resim/video olan klasörler ızgarada açılır.")),
            dropdown(
                "view",
                p,
                s.default_view,
                vec![
                    (DefaultView::Details, t!("Ayrıntılar").into()),
                    (DefaultView::Grid, t!("Izgara").into()),
                    (DefaultView::Columns, t!("Sütunlar").into()),
                    (DefaultView::Adaptive, t!("Uyarlanabilir").into()),
                ],
                |s, v| s.default_view = v,
            ),
        ),
        section(p, t!("Sıralama ve gruplama")),
        card(
            p,
            L::ArrowUpDown,
            t!("Sırala"),
            None,
            dropdown(
                "sort",
                p,
                s.default_sort,
                vec![
                    (SortKey::Name, t!("Ad").into()),
                    (SortKey::Modified, t!("Değiştirme tarihi").into()),
                    (SortKey::Created, t!("Oluşturma tarihi").into()),
                    (SortKey::Type, t!("Tür").into()),
                    (SortKey::Size, t!("Boyut").into()),
                ],
                |s, v| s.default_sort = v,
            ),
        ),
        card(p, L::ArrowDownWideNarrow, t!("Azalan sırada"), None, sw("sort-desc", p, s.default_sort_desc, |s, v| s.default_sort_desc = v)),
        card(p, L::FolderUp, t!("Önce klasörler"), None, sw("dirs-first", p, s.folders_first, |s, v| s.folders_first = v)),
        card(
            p,
            L::ListTree,
            t!("Gruplandır"),
            None,
            dropdown(
                "group",
                p,
                group_ix(&s.default_group),
                vec![(0, t!("Yok").into()), (1, t!("Ad").into()), (2, t!("Tür").into()), (3, t!("Değiştirme tarihi").into()), (4, t!("Boyut").into())],
                |s, v| s.default_group = ["none", "name", "type", "modified", "size"][v].into(),
            ),
        ),
        section(p, t!("Ayrıntılar görünümü")),
    ];
    let c = s.columns.clone();
    let pp = *p;
    v.push(expander(
        "columns",
        p,
        L::Columns3,
        t!("Sütunlar"),
        Some(t!("Ad sütunu her zaman görünür")),
        None,
        move || {
            type Set = fn(&mut Settings, bool);
            let rows: [(&str, bool, Set); 11] = [
                (t!("Değiştirme tarihi"), c.modified, |s, v| s.columns.modified = v),
                (t!("Oluşturma tarihi"), c.created, |s, v| s.columns.created = v),
                (t!("Tür"), c.kind, |s, v| s.columns.kind = v),
                (t!("Boyut"), c.size, |s, v| s.columns.size = v),
                (t!("Dosya etiketi"), c.tags, |s, v| s.columns.tags = v),
                (t!("Git durumu"), c.git, |s, v| s.columns.git = v),
                (t!("Eşitleme durumu"), c.sync, |s, v| s.columns.sync = v),
                (t!("Son commit mesajı"), c.git_message, |s, v| s.columns.git_message = v),
                (t!("Commit yazarı"), c.git_author, |s, v| s.columns.git_author = v),
                (t!("Commit tarihi"), c.git_date, |s, v| s.columns.git_date = v),
                (t!("Commit hash"), c.git_hash, |s, v| s.columns.git_hash = v),
            ];
            rows.into_iter()
                .enumerate()
                .map(|(i, (label, on, set))| sub(&pp, label, None, switch(("col-sw", i), &pp, on, move |_, cx| settings::update(cx, |s| set(s, !on)))))
                .collect()
        },
        cx,
    ));
    v
}

pub fn group_ix(g: &str) -> usize {
    match g {
        "name" => 1,
        "type" => 2,
        "modified" => 3,
        "size" => 4,
        _ => 0,
    }
}

fn files(p: &Palette, s: &Settings, cx: &mut App) -> Vec<AnyElement> {
    let mut v = vec![section(p, t!("Görünüm"))];
    let (h, sys, dot) = (s.show_hidden, s.show_system_files, s.show_dot_files);
    let pp = *p;
    v.push(expander(
        "hidden",
        p,
        L::EyeOff,
        t!("Gizli öğeler"),
        None,
        None,
        move || {
            vec![
                sub(&pp, t!("Gizli öğeleri göster"), Some("Ctrl+H"), switch("h1", &pp, h, move |_, cx| settings::update(cx, |s| s.show_hidden = !h))),
                sub(
                    &pp,
                    t!("Korunan işletim sistemi dosyalarını göster"),
                    Some(t!("desktop.ini, thumbs.db gibi sistem dosyaları")),
                    switch("h2", &pp, sys, move |_, cx| settings::update(cx, |s| s.show_system_files = !sys)),
                ),
                sub(
                    &pp,
                    t!("Nokta ile başlayan dosyaları göster"),
                    Some(t!("Kapalıyken .git, .config gibi öğeler gizli sayılır")),
                    switch("h3", &pp, dot, move |_, cx| settings::update(cx, |s| s.show_dot_files = !dot)),
                ),
            ]
        },
        cx,
    ));
    v.push(card(p, L::FileType, t!("Dosya uzantılarını göster"), None, sw("ext", p, s.show_extensions, |s, v| s.show_extensions = v)));
    v.push(card(p, L::Image, t!("Küçük resimleri göster"), None, sw("thumbs", p, s.show_thumbnails, |s, v| s.show_thumbnails = v)));
    v.push(card(p, L::SquareCheck, t!("Seçerken onay kutularını göster"), None, sw("checkboxes", p, s.show_checkboxes, |s, v| s.show_checkboxes = v)));

    v.push(section(p, t!("Davranışlar")));
    v.push(card(
        p,
        L::MousePointerClick,
        t!("Öğeleri tek tıklamayla aç"),
        None,
        dropdown(
            "single-click",
            p,
            s.single_click,
            vec![(ClickPref::Never, t!("Asla").into()), (ClickPref::Folders, t!("Yalnızca klasörler").into()), (ClickPref::Always, t!("Her zaman").into())],
            |s, v| s.single_click = v,
        ),
    ));
    v.push(card(p, L::SquarePlus, t!("Klasörleri yeni sekmede aç"), None, sw("dir-newtab", p, s.open_folders_in_new_tab, |s, v| s.open_folders_in_new_tab = v)));
    v.push(card(
        p,
        L::Trash,
        t!("Silmeden önce onay iste"),
        None,
        dropdown(
            "confirm-del",
            p,
            s.confirm_delete,
            vec![
                (DeletePref::Always, t!("Her zaman").into()),
                (DeletePref::PermanentOnly, t!("Yalnızca kalıcı silmede").into()),
                (DeletePref::Never, t!("Asla").into()),
            ],
            |s, v| s.confirm_delete = v,
        ),
    ));
    v.push(card(p, L::TriangleAlert, t!("Uzantı değişince uyar"), None, sw("ext-warn", p, s.warn_extension_change, |s, v| s.warn_extension_change = v)));
    v.push(card(p, L::MousePointer, t!("Üzerine gelince seç"), None, sw("hover-sel", p, s.select_on_hover, |s, v| s.select_on_hover = v)));
    v.push(card(p, L::ArrowUp, t!("Boş alana çift tıklayınca üst klasöre çık"), None, sw("dbl-up", p, s.double_click_up, |s, v| s.double_click_up = v)));
    v.push(card(p, L::FolderUp, t!("Yukarı çıkınca önceki klasörü seç"), None, sw("prev-sel", p, s.select_previous_on_up, |s, v| s.select_previous_on_up = v)));
    v.push(card(
        p,
        L::FileArchive,
        t!("Arşivleri klasör gibi aç"),
        Some(t!("ZIP, 7z ve tar dosyalarının içine çift tıklayarak girilir; içindeki dosyalar açılınca geçici bir klasöre çıkarılır")),
        sw("browse-arc", p, s.browse_archives, |s, v| s.browse_archives = v),
    ));
    v.push(card(
        p,
        L::Search,
        t!("Aramada Windows dizinini kullan"),
        Some(t!("Dizinlenmiş klasörlerde dosya içerikleri de aranır ve kind:picture, size:>10MB gibi filtreler çalışır")),
        sw("search-index", p, s.search_index, |s, v| s.search_index = v),
    ));
    v.push(card(
        p,
        L::Zap,
        t!("Hızlı arama dizini"),
        Some(&name_index_status()),
        sw("name-index", p, s.name_index, |s, v| s.name_index = v),
    ));
    if s.name_index {
        v.push(index_exclusions(p, s));
    }
    v.push(card(
        p,
        L::HardDrive,
        t!("Boyut biçimi"),
        None,
        dropdown(
            "size-fmt",
            p,
            s.decimal_sizes,
            vec![(false, t!("İkili (1 KB = 1024 bayt)").into()), (true, t!("Ondalık (1 KB = 1000 bayt)").into())],
            |s, v| s.decimal_sizes = v,
        ),
    ));
    let pp = *p;
    v.push(expander(
        "folder-sizes",
        p,
        L::FolderSearch,
        t!("Klasör boyutlarını hesapla"),
        Some(t!("Boyut sütununda klasörlerin toplam boyutu gösterilir")),
        Some(sw("calc-sizes", p, s.calculate_folder_sizes, |s, v| s.calculate_folder_sizes = v)),
        move || {
            vec![
                h_flex()
                    .mx_4()
                    .my_2()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .rounded(theme::RADIUS_SM)
                    .bg(hsla_alpha(0xFCE100, 0.12))
                    .child(Icon::new(L::CircleAlert).size(px(14.)).text_color(rgb(0xFCE100)))
                    .child(div().text_xs().text_color(pp.text).child(t!("Büyük klasörlerde disk ve işlemci yükü artabilir; hesap arka planda yapılır, arayüz donmaz.")))
                    .into_any_element(),
            ]
        },
        cx,
    ));

    v.push(card(
        p,
        L::HardDrive,
        t!("Klasörlerde disk üzerindeki boyutu göster"),
        Some(t!("Boyut sütunu klasörler için dosya boyutlarının toplamı yerine kümelere yuvarlanmış boyutu gösterir; sıralama da buna göre yapılır")),
        sw("folder-on-disk", p, s.folder_size_on_disk, |s, v| s.folder_size_on_disk = v),
    ));

    // Files' folder aliases: "@name" in the address box
    v.push(section(p, t!("Klasör takma adları")));
    let aliases: Vec<(String, std::path::PathBuf)> = s.folder_aliases.iter().map(|(n, p)| (n.clone(), p.clone())).collect();
    let count = aliases.len();
    v.push(expander(
        "aliases",
        p,
        L::AtSign,
        t!("Takma adlar"),
        Some(t!("Adres çubuğuna @ad yazarak klasöre gidin. Eklemek için bir klasöre sağ tıklayıp \"Takma ad ver…\" seçin.")),
        Some(div().text_sm().text_color(p.text_secondary).child(count.to_string()).into_any_element()),
        move || {
            if aliases.is_empty() {
                return vec![sub(&pp, t!("Henüz takma ad yok"), None, div().into_any_element())];
            }
            aliases
                .into_iter()
                .enumerate()
                .map(|(i, (name, path))| {
                    let key = name.clone();
                    sub(
                        &pp,
                        &format!("@{name}"),
                        Some(&path.to_string_lossy()),
                        button(("alias-rm", i), &pp, t!("Kaldır"), move |_, cx| {
                            let key = key.clone();
                            settings::update(cx, move |s| {
                                s.folder_aliases.remove(&key);
                            });
                        }),
                    )
                })
                .collect()
        },
        cx,
    ));
    v
}

fn actions(p: &Palette, cx: &mut App) -> Vec<AnyElement> {
    let mut grid = div().px_4().py_3().flex().flex_wrap().gap_x_6().gap_y_0p5();
    let capturing = crate::keymap_editor::capturing(cx);
    for (i, (label, keys, action)) in crate::command_palette::shortcut_entries().into_iter().enumerate() {
        let custom = crate::keymap_editor::custom_keys(&action, cx);
        let waiting = capturing.as_deref() == Some(action.as_str());
        let shown: SharedString = if waiting {
            t!("Tuşlara basın…").into()
        } else if let Some(k) = &custom {
            crate::keymap_editor::pretty(k).into()
        } else if keys.is_empty() {
            "—".into()
        } else {
            keys.into()
        };
        let act = action.clone();
        let reset_act = action.clone();
        grid = grid.child(
            h_flex()
                .id(("kb", i))
                .w(px(380.))
                .h(px(32.))
                .px_2()
                .rounded(theme::RADIUS_SM)
                .justify_between()
                .items_center()
                .text_sm()
                .cursor_pointer()
                .hover(|s| s.bg(p.pressed))
                .when(waiting, |d| d.bg(p.selection))
                .on_click(move |_, window, cx| {
                    crate::keymap_editor::start(act.clone(), cx);
                    window.refresh();
                })
                .child(crate::i18n::t(label))
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(
                            div()
                                .px_2()
                                .py_0p5()
                                .rounded(px(4.))
                                .border_1()
                                .border_color(p.border)
                                .text_xs()
                                .text_color(if waiting || custom.is_some() { p.accent } else { p.text_secondary })
                                .child(shown),
                        )
                        .when(custom.is_some() && !waiting, |d| {
                            d.child(
                                div()
                                    .id(("kb-reset", i))
                                    .px_1()
                                    .rounded(theme::RADIUS_SM)
                                    .text_xs()
                                    .text_color(p.text_secondary)
                                    .hover(|s| s.bg(p.pressed))
                                    .child("↺")
                                    .on_click(move |_, window, cx| {
                                        cx.stop_propagation();
                                        crate::keymap_editor::reset(&reset_act, cx);
                                        window.refresh();
                                    }),
                            )
                        }),
                ),
        );
    }
    vec![
        shell(p)
            .child(card_body(
                p,
                L::Keyboard,
                t!("Klavye kısayolları").into(),
                Some(t!("Değiştirmek için bir komuta tıklayıp yeni tuşlara basın (Esc: vazgeç). ↺ varsayılana döndürür.").into()),
                None,
            ))
            .child(div().h(px(1.)).bg(p.divider))
            .child(grid)
            .into_any_element(),
    ]
}

fn tags(p: &Palette, cx: &mut App) -> Vec<AnyElement> {
    let defs = cx.global::<crate::tags::Tags>().0.defs.clone();
    let mut list = v_flex().py_1();
    for d in defs {
        let (id, name, color) = (d.id, d.name.clone(), d.color);
        let rename_name = name.clone();
        list = list.child(
            h_flex()
                .min_h(px(48.))
                .pl(px(56.))
                .pr_4()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .id(("tag-color", id as usize))
                        .size(px(18.))
                        .rounded_full()
                        .bg(rgb(color))
                        .cursor_pointer()
                        .on_click(move |ev: &ClickEvent, window, cx| {
                            let items = TAG_COLORS
                                .iter()
                                .map(|(n, c)| {
                                    let c = *c;
                                    Item::action(crate::i18n::t(n), MenuIcon::Dot(rgb(c).into()), move |_, cx| crate::tags::update(cx, |t| t.recolor(id, c)))
                                        .checked(c == color)
                                })
                                .collect();
                            fluent_menu::show(Menu { quick: Vec::new(), items }, ev.position(), window, cx);
                        }),
                )
                .child(div().flex_1().text_sm().child(crate::i18n::td(&name)))
                .child(button(("tag-edit", id as usize), p, t!("Yeniden adlandır"), move |window, cx| {
                    tag_name_dialog(Some((id, rename_name.clone())), window, cx)
                }))
                .child(button(("tag-del", id as usize), p, t!("Sil"), move |window, cx| {
                    crate::prompt::show(
                        t!("Etiket silinsin mi?"),
                        Some(t!("Etiket tüm öğelerden kaldırılır. Dosyalar silinmez.").to_string()),
                        None,
                        vec![
                            crate::prompt::Button::new(t!("Sil"), crate::prompt::Kind::Danger, move |_, _, cx| {
                                crate::tags::update(cx, |t| t.remove(id));
                                Ok(())
                            }),
                            crate::prompt::Button::cancel(),
                        ],
                        window,
                        cx,
                    )
                })),
        );
    }
    vec![
        shell(p)
            .child(card_body(
                p,
                L::Tag,
                t!("Etiketler").into(),
                Some(t!("Renkli etiketlerle dosyaları klasörlerden bağımsız gruplayın. Renge tıklayarak değiştirin.").into()),
                Some(button("tag-new", p, t!("Yeni etiket"), |window, cx| tag_name_dialog(None, window, cx))),
            ))
            .child(div().h(px(1.)).bg(p.divider))
            .child(list)
            .into_any_element(),
    ]
}

const TAG_COLORS: [(&str, u32); 8] = [
    ("Kırmızı", 0xFF453A),
    ("Turuncu", 0xFF9F0A),
    ("Sarı", 0xFFD60A),
    ("Yeşil", 0x30D158),
    ("Turkuaz", 0x40C8E0),
    ("Mavi", 0x0A84FF),
    ("Mor", 0xBF5AF2),
    ("Gri", 0x8E8E93),
];

fn tag_name_dialog(existing: Option<(u32, String)>, window: &mut Window, cx: &mut App) {
    let title = if existing.is_some() { t!("Etiketi yeniden adlandır") } else { t!("Yeni etiket") };
    let initial = existing.as_ref().map(|e| e.1.clone()).unwrap_or_default();
    crate::prompt::show(
        title,
        None,
        Some((initial, t!("Etiket adı").to_string())),
        vec![
            crate::prompt::Button::new(t!("Kaydet"), crate::prompt::Kind::Primary, move |value, _, cx| {
                let name = value.unwrap_or_default().trim().to_string();
                if name.is_empty() {
                    return Err(t!("Bir ad yazın").to_string());
                }
                match existing {
                    Some((id, _)) => crate::tags::update(cx, |t| t.rename(id, &name)),
                    None => crate::tags::update(cx, |t| {
                        t.add(&name, TAG_COLORS[t.defs.len() % TAG_COLORS.len()].1);
                    }),
                }
                Ok(())
            }),
            crate::prompt::Button::cancel(),
        ],
        window,
        cx,
    );
}

fn dev(p: &Palette, s: &Settings) -> Vec<AnyElement> {
    let ide_label = if s.ide_path.is_empty() { t!("VS Code (otomatik)").to_string() } else { s.ide_path.clone() };
    vec![
        card(
            p,
            L::Code,
            t!("Durum çubuğunda \"Düzenleyicide aç\" düğmesi"),
            None,
            dropdown(
                "ide-btn",
                p,
                s.ide_button,
                vec![
                    (IdePref::GitRepos, t!("Git depolarında göster").into()),
                    (IdePref::Always, t!("Her zaman göster").into()),
                    (IdePref::Never, t!("Gösterme").into()),
                ],
                |s, v| s.ide_button = v,
            ),
        ),
        card(
            p,
            L::SquareTerminal,
            t!("Düzenleyici"),
            Some(&ide_label),
            h_flex()
                .gap_2()
                .child(button("ide-browse", p, t!("Gözat"), |window, cx| pick_into(window, cx, t!("Düzenleyici seç"), PickTarget::Ide)))
                .child(button("ide-reset", p, t!("Otomatik"), |_, cx| settings::update(cx, |s| s.ide_path.clear())))
                .into_any_element(),
        ),
    ]
}

/// DirectLook ayarı anahtarı (ayrı dosya `directlook.toml`; değişince DirectLook'a haber verilir).
fn lsw(id: &'static str, p: &Palette, on: bool, set: fn(&mut LookSettings, bool)) -> AnyElement {
    switch(id, p, on, move |_, cx| look_update(cx, |s| set(s, !on)))
}

fn look_update(cx: &mut App, f: impl FnOnce(&mut LookSettings)) {
    crate::look::config::update(cx, f);
    crate::look::ipc::send(crate::look::ipc::Msg::Reload, false);
}

fn ldropdown<T: Copy + PartialEq + 'static>(id: &'static str, p: &Palette, current: T, options: Vec<(T, String)>, set: fn(&mut LookSettings, T)) -> AnyElement {
    let label = options.iter().find(|(v, _)| *v == current).map(|(_, l)| l.clone()).unwrap_or_default();
    select_box(id, p, label, move |_| {
        options.iter().map(|(v, l)| {
            let v = *v;
            Choice::new(l.clone(), v == current, move |_, cx| look_update(cx, |s| set(s, v)))
        }).collect()
    })
}

/// DirectLook (Hızlı Bakış, ayrı program) ayarları.
fn look(p: &Palette, s: &Settings, cx: &mut App) -> Vec<AnyElement> {
    use crate::settings::{LookBackdrop, LookImageTheme};
    // DirectLook da yazabilir: sayfa her açılışta dosyadan okunur
    let l = LookSettings::load();
    let enabled = s.directlook_enabled;
    let mut v = vec![card(
        p,
        L::Eye,
        t!("DirectLook'u kullan"),
        Some(t!("Boşluk ile önizleme. Ayrı bir program olarak çalışır: smoothyfiles'ı yavaşlatmaz, biri kapanınca öteki etkilenmez.")),
        switch("dl-enabled", p, enabled, move |_, cx| {
            settings::update(cx, |s| s.directlook_enabled = !enabled);
            if enabled {
                crate::look::ipc::send(crate::look::ipc::Msg::Quit, false);
            } else {
                crate::look::ipc::send(crate::look::ipc::Msg::Wake, true);
            }
        }),
    )];
    if !enabled {
        return v;
    }
    v.push(card(
        p,
        L::Monitor,
        t!("Explorer ve masaüstünde de Boşluk ile önizle"),
        Some(t!("DirectLook arka planda çalışır ve Windows başladığında açılır (bildirim alanında simge); smoothyfiles kapalıyken de")),
        switch("look-wide", p, l.look_system_wide, {
            let on = l.look_system_wide;
            move |_, cx| {
                look_update(cx, |s| s.look_system_wide = !on);
                if let Some(exe) = crate::look::ipc::exe() {
                    if let Err(e) = rf_win::dialogs::set_start_with_windows_as("DirectLook", &exe, &["--background"], !on) {
                        log::warn!("DirectLook start with Windows: {e}");
                    }
                }
                crate::look::ipc::send(crate::look::ipc::Msg::Wake, true);
            }
        }),
    ));
    if l.look_system_wide && !l.look_ignore_quicklook && crate::look::system::quicklook_running() {
        v.push(card(
            p,
            L::TriangleAlert,
            t!("QuickLook çalışıyor"),
            Some(t!("İki uygulama aynı tuşu dinlemesin diye sistem geneli kısayol başlatılmadı")),
            button("look-force", p, t!("Yine de aç"), |window, cx| {
                look_update(cx, |s| s.look_ignore_quicklook = true);
                window.refresh();
            }),
        ));
    }
    let _ = cx;
    v.extend([
        section(p, t!("Pencere")),
        card(
            p,
            L::Layers,
            t!("Arka plan"),
            None,
            ldropdown(
                "look-backdrop",
                p,
                l.look_backdrop,
                vec![
                    (LookBackdrop::Mica, "Mica".into()),
                    (LookBackdrop::Acrylic, "Acrylic".into()),
                    (LookBackdrop::Tabbed, t!("Mica (sekmeli)").into()),
                    (LookBackdrop::None, t!("Düz renk").into()),
                ],
                |s, v| s.look_backdrop = v,
            ),
        ),
        card(p, L::PictureInPicture, t!("Her zaman üstte"), None, lsw("look-top", p, l.look_topmost, |s, v| s.look_topmost = v)),
        card(p, L::PanelBottom, t!("Görev çubuğunda göster"), None, lsw("look-taskbar", p, l.look_show_in_taskbar, |s, v| s.look_show_in_taskbar = v)),
        card(
            p,
            L::AppWindow,
            t!("Bildirim alanında DirectLook simgesi"),
            Some(t!("smoothyfiles arka planda çalışırken gösterilmez: tek simge smoothyfiles'ınkidir")),
            lsw("look-tray", p, l.look_tray_icon, |s, v| s.look_tray_icon = v),
        ),
        card(p, L::MousePointerClick, t!("Açılınca pencereyi odakla"), Some(t!("Kapalıyken klavye Explorer'da kalır: oklarla dosyalar arasında gezinilir")), lsw("look-focus", p, l.look_focus_on_open, |s, v| s.look_focus_on_open = v)),
        card(p, L::CircleX, t!("Başka pencereye geçince kapat"), Some(t!("Sabitlenmiş pencereler açık kalır")), lsw("look-lostfocus", p, l.look_close_on_lost_focus, |s, v| s.look_close_on_lost_focus = v)),
        card(p, L::Zap, t!("Geçiş animasyonu"), None, lsw("look-transition", p, l.look_transition, |s, v| s.look_transition = v)),
        card(p, L::RefreshCw, t!("Yeniden yükle düğmesi"), None, lsw("look-reload", p, l.look_show_reload, |s, v| s.look_show_reload = v)),
        card(p, L::FolderSync, t!("Dosya değişince yeniden yükle"), None, lsw("look-autoreload", p, l.look_auto_reload, |s, v| s.look_auto_reload = v)),
        section(p, t!("İçerik")),
        card(
            p,
            L::Image,
            t!("Resim zemini"),
            None,
            ldropdown(
                "look-imgtheme",
                p,
                l.look_image_theme,
                vec![(LookImageTheme::Dark, t!("Koyu").into()), (LookImageTheme::Light, t!("Açık").into())],
                |s, v| s.look_image_theme = v,
            ),
        ),
        card(p, L::RotateCw, t!("Videoları döngüde oynat"), None, lsw("look-loop", p, l.look_loop, |s, v| s.look_loop = v)),
        card(p, L::List, t!("Metinde satır numaraları"), Some(t!("Önizlemede Alt+L")), lsw("look-lines", p, l.look_text_line_numbers, |s, v| s.look_text_line_numbers = v)),
        card(p, L::TextAlignJustify, t!("Metinde sözcük kaydırma"), Some(t!("Önizlemede Alt+Z")), lsw("look-wrap", p, l.look_text_wrap, |s, v| s.look_text_wrap = v)),
    ]);
    // text font and size
    let font_label = if l.look_text_font.is_empty() { t!("Varsayılan").to_string() } else { l.look_text_font.clone() };
    let cur_font = l.look_text_font.clone();
    let fonts = font_choices(cx);
    v.push(card(
        p,
        L::Type,
        t!("Metin yazı tipi"),
        None,
        select_box("look-font", p, font_label, move |_| {
            std::iter::once(String::new())
                .chain(fonts.iter().cloned())
                .map(|f| {
                    let label = if f.is_empty() { t!("Varsayılan").to_string() } else { f.clone() };
                    let sel = f == cur_font;
                    Choice::new(label, sel, move |_, cx| {
                        let f = f.clone();
                        look_update(cx, |s| s.look_text_font = f)
                    })
                })
                .collect()
        }),
    ));
    let cur_size = l.look_text_font_size.round() as u32;
    v.push(card(
        p,
        L::ALargeSmall,
        t!("Metin boyutu"),
        None,
        select_box("look-font-size", p, format!("{cur_size} px"), move |_| {
            [11u32, 12, 13, 14, 15, 16, 18, 20, 24]
                .into_iter()
                .map(|n| Choice::new(format!("{n} px"), n == cur_size, move |_, cx| look_update(cx, |s| s.look_text_font_size = n as f32)))
                .collect()
        }),
    ));
    // which file types DirectLook previews
    v.push(card(
        p,
        L::ListFilter,
        t!("Önizlenecek dosyalar"),
        None,
        ldropdown(
            "look-ext-mode",
            p,
            l.look_ext_allow_mode,
            vec![(false, t!("Hepsi, listedekiler hariç").to_string()), (true, t!("Yalnızca listedekiler").to_string())],
            |s, v| s.look_ext_allow_mode = v,
        ),
    ));
    let allow = l.look_ext_allow_mode;
    let list = if allow { l.look_ext_allowlist.clone() } else { l.look_ext_blocklist.clone() };
    let shown = if list.trim().is_empty() { "—".to_string() } else { list.clone() };
    v.push(card(
        p,
        L::CircleSlash,
        if allow { t!("Önizlenecek uzantılar") } else { t!("Önizlenmeyecek uzantılar") },
        Some(&shown),
        button("look-ext-edit", p, t!("Düzenle…"), move |window, cx| {
            let ok = crate::prompt::Button::new(t!("Kaydet"), crate::prompt::Kind::Primary, move |text, _, cx| {
                let text = normalize_exts(&text.unwrap_or_default());
                look_update(cx, |s| if allow { s.look_ext_allowlist = text } else { s.look_ext_blocklist = text });
                Ok(())
            });
            crate::prompt::show(
                if allow { t!("Önizlenecek uzantılar") } else { t!("Önizlenmeyecek uzantılar") },
                Some(t!("Noktalı virgülle ayırın, örneğin: .iso; .insv; .psd").to_string()),
                Some((list.clone(), ".iso; .insv".to_string())),
                vec![ok, crate::prompt::Button::cancel()],
                window,
                cx,
            );
        }),
    ));
    v
}

/// "iso, .PSD ;; insv" → ".iso;.psd;.insv": dots added, lowercase, no duplicates.
fn normalize_exts(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for part in text.split([';', ',', ' ']) {
        let p = part.trim().trim_start_matches('*').to_lowercase();
        if p.is_empty() || p == "." {
            continue;
        }
        let e = if p.starts_with('.') { p } else { format!(".{p}") };
        if !out.contains(&e) {
            out.push(e);
        }
    }
    out.join(";")
}

#[cfg(test)]
mod ext_tests {
    #[test]
    fn extension_lists_are_normalized() {
        assert_eq!(super::normalize_exts("iso, .PSD ;; insv *.Mkv .iso"), ".iso;.psd;.insv;.mkv");
        assert_eq!(super::normalize_exts("  "), "");
    }
}

fn advanced(p: &Palette, s: &Settings) -> Vec<AnyElement> {
    let start_on = rf_win::dialogs::starts_with_windows();
    let load = hybrid::load_snapshot();
    let pct = |v: Option<f32>| v.map(crate::i18n::pct).unwrap_or_else(|| "—".into());
    let live = format!(
        "{} {} · {} {} · {} {} · {} {:.1} ms · {} {}",
        t!("Şu an:"),
        hybrid::current_backend_label(),
        t!("GPU yükü"),
        pct(load.gpu_percent),
        t!("CPU yükü"),
        pct(load.cpu_percent),
        t!("son kare"),
        load.last_frame_ms,
        hybrid::switch_count(),
        t!("geçiş")
    );
    let dfm_on = crate::DEFAULT_FM.load(std::sync::atomic::Ordering::Relaxed);
    let dialogs_on = crate::FILE_DIALOGS.load(std::sync::atomic::Ordering::Relaxed);
    // (only looks: the copy is made when the switch is turned on)
    let has_dll = std::env::current_exe().is_ok_and(|e| e.with_file_name(rf_win::dialog_reg::DLL_NAME).exists());
    // while smoothyfiles is the default file manager: archives can be given to it too (only the
    // user can pick the default app, in Windows' own page)
    let archives = dfm_on.then(|| {
        card(
            p,
            L::FileArchive,
            t!("Arşivleri smoothyfiles ile aç"),
            Some(t!("Zip, 7z, rar, tar için varsayılan uygulama Windows'ta seçilir. Açılan sayfada smoothyfiles'ı varsayılan yapın.")),
            button("dfm-arch", p, t!("Varsayılan uygulamalar"), |_, _| {
                let page = format!("ms-settings:defaultapps?registeredAppUser={}", rf_win::default_fm::APP_NAME);
                if let Err(e) = rf_win::shell::open(std::path::Path::new(&page)) {
                    log::warn!("default apps page: {e:#}");
                }
            }),
        )
    });
    let mut out = vec![
        link_row(p, "export", L::Upload, t!("Ayarları dışa aktar"), None, |window, cx| pick_into(window, cx, t!("Ayarları dışa aktar"), PickTarget::Export)),
        link_row(p, "import", L::Download, t!("Ayarları içe aktar"), None, |window, cx| pick_into(window, cx, t!("Ayarları içe aktar"), PickTarget::Import)),
        link_row(p, "edit-file", L::FilePen, t!("Ayar dosyasını düzenle"), Some(t!("settings.toml metin düzenleyicide açılır")), |_, _| {
            if let Some(path) = settings::path() {
                std::thread::spawn(move || {
                    let _ = std::process::Command::new("notepad.exe").arg(path).spawn();
                });
            }
        }),
        card(
            p,
            L::Power,
            t!("Windows başladığında smoothyfiles'ı aç"),
            None,
            switch("start-win", p, start_on, move |window, _| {
                if let Ok(exe) = std::env::current_exe() {
                    if let Err(e) = rf_win::dialogs::set_start_with_windows(&exe, !start_on) {
                        log::warn!("start with Windows: {e}");
                    }
                }
                window.refresh();
            }),
        ),
        card(
            p,
            L::PanelBottom,
            t!("Pencere kapatılınca arka planda çalışmaya devam et"),
            Some(t!("Bildirim alanında bir simge kalır; yeni pencere anında açılır")),
            sw("background", p, s.run_in_background, |s, v| s.run_in_background = v),
        ),
        card(
            p,
            L::Bell,
            t!("Uzun işlemler bitince bildirim göster"),
            Some(t!("Kopyalama, taşıma, sıkıştırma 10 saniyeden uzun sürdüyse ve smoothyfiles önde değilse")),
            sw("notify-ops", p, s.notify_long_ops, |s, v| s.notify_long_ops = v),
        ),
        section(p, t!("Performans")),
        card(
            p,
            L::Zap,
            t!("Hızlı dosya işlemleri"),
            Some(t!("Kopyalama, başka sürücüye taşıma ve kalıcı silme kendi paralel motorumuzla. Kapalıyken her şey Windows'un motoruyla yapılır.")),
            sw("fast-ops", p, s.fast_file_ops, |s, v| s.fast_file_ops = v),
        ),
        card(
            p,
            L::ShieldCheck,
            t!("Kopyaları doğrula"),
            Some(t!("Kopyalama bitince her dosya kaynağıyla karşılaştırılır. Tam doğrulama kopyayı diskten okur; daha uzun sürer.")),
            dropdown(
                "copy-verify",
                p,
                s.copy_verify,
                vec![
                    (crate::settings::CopyVerify::Off, t!("Kapalı").into()),
                    (crate::settings::CopyVerify::Quick, t!("Hızlı: boyut ve tarih").into()),
                    (crate::settings::CopyVerify::Full, t!("Tam: içerik").into()),
                ],
                |s, v| s.copy_verify = v,
            ),
        ),
        card(
            p,
            L::Gauge,
            t!("Kopyalama hız sınırı"),
            Some(t!("Ağa veya USB belleğe kopyalarken bağlantıyı tıkamamak için.")),
            dropdown(
                "copy-limit",
                p,
                s.copy_speed_limit,
                std::iter::once((0, t!("Sınırsız").to_string())).chain([5u32, 10, 25, 50, 100, 250].into_iter().map(|m| (m, format!("{m} MB/s")))).collect(),
                |s, v| s.copy_speed_limit = v,
            ),
        ),
        card(
            p,
            L::Activity,
            t!("Durum çubuğunda performans bilgisi"),
            Some(t!("Klasörün listelenme süresi ve çizim yöntemi (GPU / CPU)")),
            sw("perf-info", p, s.show_perf_info, |s, v| s.show_perf_info = v),
        ),
        shell(p)
            .child(card_body(
                p,
                L::Activity,
                t!("Çizim yöntemi").into(),
                Some(t!("Otomatik: açılışta ekran kartı denetlenir, sorun yoksa arayüz GPU ile çizilir. GPU doluyken (oyun, render) CPU'ya geçer, boşalınca geri döner. Geçiş fark edilmez.").into()),
                Some(dropdown(
                    "renderer",
                    p,
                    s.renderer,
                    vec![(RendererPref::Auto, t!("Otomatik").into()), (RendererPref::Gpu, "GPU".into()), (RendererPref::Cpu, "CPU".into())],
                    |s, v| s.renderer = v,
                )),
            ))
            .child(div().pl(px(56.)).pr_4().pb_3().text_xs().text_color(p.text_tertiary).child(live))
            .into_any_element(),
        section(p, t!("Deneysel")),
        card(
            p,
            L::AppWindow,
            t!("Varsayılan dosya yöneticisi olarak ayarla"),
            Some(t!("Klasörler ve Win+E smoothyfiles ile açılır. Yalnızca bu kullanıcı için; istediğiniz an geri alınır.")),
            switch("dfm", p, dfm_on, move |window, _| {
                let exe = std::env::current_exe().map(|e| e.display().to_string()).unwrap_or_default();
                std::thread::spawn(move || {
                    let classes = rf_win::default_fm::CLASSES;
                    let r = if dfm_on { rf_win::default_fm::disable(classes) } else { rf_win::default_fm::enable(&exe, classes) };
                    if let Err(e) = r {
                        log::warn!("default file manager: {e:#}");
                    }
                    crate::DEFAULT_FM.store(rf_win::default_fm::is_enabled(classes), std::sync::atomic::Ordering::Relaxed);
                });
                window.refresh();
            }),
        ),
    ];
    out.extend(archives);
    // other apps' Open / Save / folder dialogs show smoothyfiles (needs smoothyfiles_dialog.dll)
    out.push(card(
        p,
        L::FolderOpen,
        t!("Aç / Kaydet pencerelerinde smoothyfiles"),
        Some(if has_dll {
            t!("Uygulamaların dosya açma, kaydetme ve klasör seçme pencereleri smoothyfiles ile açılır. Pencereye kendi seçeneklerini ekleyen uygulamalarda Windows'un penceresi açılır. Açık uygulamalarda, yeniden başlatınca geçerli olur.")
        } else {
            t!("smoothyfiles_dialog.dll bulunamadı: smoothyfiles'ı kurulum betiğiyle kurun.")
        }),
        switch("dlg", p, dialogs_on, move |window, _| {
            if !has_dll && !dialogs_on {
                return;
            }
            std::thread::spawn(move || {
                let classes = rf_win::dialog_reg::CLASSES;
                let r = match (dialogs_on, crate::dialog_dll()) {
                    (true, _) => rf_win::dialog_reg::disable(classes),
                    (false, Some(dll)) => rf_win::dialog_reg::enable(&dll, classes),
                    (false, None) => Ok(()),
                };
                if let Err(e) = r {
                    log::warn!("file dialogs: {e:#}");
                }
                crate::FILE_DIALOGS.store(rf_win::dialog_reg::is_enabled(classes), std::sync::atomic::Ordering::Relaxed);
            });
            window.refresh();
        }),
    ));
    out.extend([
        card(
            p,
            L::FolderTree,
            t!("Düzleştirme seçeneğini göster"),
            Some(t!("Klasördeki tüm alt klasörlerin dosyalarını tek seviyeye taşır. Ctrl+Z ile geri alınır.")),
            sw("flatten", p, s.show_flatten, |s, v| s.show_flatten = v),
        ),
    ]);
    out
}

fn open_url(url: &'static str) {
    std::thread::spawn(move || {
        let _ = rf_win::shell::open(std::path::Path::new(url));
    });
}

/// Update status and the button that fits it (Settings → About).
fn update_card(p: &Palette, s: &Settings) -> AnyElement {
    use crate::installer::State;
    let state = crate::installer::state();
    let text = match &state {
        State::NotInstalled => t!("Bu kopya kurulum programıyla kurulmadı: güncellemeleri GitHub'dan indirin.").to_string(),
        State::Idle => t!("Güncellemeler GitHub'dan gelir.").to_string(),
        State::Checking => t!("Denetleniyor…").to_string(),
        State::UpToDate => t!("smoothyfiles güncel.").to_string(),
        State::Downloading(v, pct) => format!("{} {v} · %{pct}", t!("İndiriliyor:")),
        State::Ready(v) => format!("{} {v}. {}", t!("İndirildi:"), t!("Yeniden başlatınca uygulanır.")),
        State::Failed(e) => format!("{} {e}", t!("Denetlenemedi:")),
    };
    let control = match state {
        State::NotInstalled => button("upd-web", p, t!("Sürümler sayfası"), |_, _| open_url(RELEASES_URL)),
        State::Ready(_) => button("upd-restart", p, t!("Yeniden başlat ve güncelle"), |_, _| {
            std::thread::spawn(crate::installer::restart_to_update);
        }),
        State::Checking | State::Downloading(..) => div().into_any_element(),
        _ => button("upd-check", p, t!("Güncellemeleri denetle"), |window, cx| {
            check_updates_now(cx);
            window.refresh();
        }),
    };
    let _ = s;
    let icon = match state {
        State::Downloading(..) => L::Download,
        State::Ready(_) => L::CircleCheck,
        _ => L::RefreshCw,
    };
    // behind the row: a light sweeping while checking, a bar filling while downloading, full when done
    let tint = p.accent;
    let layer = match state {
        State::Checking => Some(
            glow(tint, 0.22)
                .w(relative(0.3))
                .with_animation(
                    "upd-sweep",
                    Animation::new(std::time::Duration::from_millis(1200)).repeat().with_easing(ease_in_out),
                    |el, t| el.left(relative(-0.3 + 1.3 * t)),
                )
                .into_any_element(),
        ),
        State::Downloading(_, pct) => Some(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(relative(pct as f32 / 100.0))
                .overflow_hidden()
                .bg(tint.opacity(0.18))
                .child(
                    // a soft highlight running along the filled part
                    glow(tint, 0.2)
                        .w(px(120.))
                        .with_animation(
                            "upd-run",
                            Animation::new(std::time::Duration::from_millis(1000)).repeat(),
                            |el, t| el.left(relative(-0.2 + 1.2 * t)),
                        ),
                )
                .into_any_element(),
        ),
        State::Ready(_) => Some(div().absolute().inset_0().bg(tint.opacity(0.14)).into_any_element()),
        _ => None,
    };
    shell(p)
        .relative()
        .overflow_hidden()
        .when(matches!(state, State::Ready(_)), |d| d.border_color(tint.opacity(0.6)))
        .children(layer)
        .child(card_body(p, icon, t!("Güncellemeler").into(), Some(text.into()), Some(control)))
        .into_any_element()
}

/// A soft band of light: transparent at both edges, `alpha` in the middle.
fn glow(c: Hsla, alpha: f32) -> Div {
    let half = |from: f32, to: f32| {
        div().flex_1().h_full().bg(linear_gradient(90., linear_color_stop(c.opacity(from), 0.), linear_color_stop(c.opacity(to), 1.)))
    };
    div().absolute().top_0().bottom_0().flex().child(half(0., alpha)).child(half(alpha, 0.))
}

/// "Check for updates": runs off the UI thread and redraws while it checks and downloads.
pub(crate) fn check_updates_now(cx: &mut App) {
    cx.spawn(async move |cx| {
        let run = cx.background_executor().spawn(async { crate::installer::check() });
        crate::installer::redraw_while_busy(cx).await;
        run.await;
        cx.update(|cx| cx.refresh_windows());
    })
    .detach();
}

const REPO_URL: &str = "https://github.com/latiefahmad/smoothyfiles";
const SITE_URL: &str = "https://latief.id/smoothyfiles";
const DISCORD_URL: &str = "https://discord.gg/K2banZ36zJ";
const ISSUES_URL: &str = "https://github.com/latiefahmad/smoothyfiles/issues/new";
const RELEASES_URL: &str = "https://github.com/latiefahmad/smoothyfiles/releases";

fn about(p: &Palette, cx: &mut App) -> Vec<AnyElement> {
    let ver = crate::installer::version_full();
    let version = format!("{} {ver}", t!("Sürüm:"));
    let pp = *p;
    let s = cx.global::<Settings>().clone();
    vec![
        card(
            p,
            L::FolderOpen,
            "smoothyfiles",
            Some(&version),
            button("copy-ver", p, t!("Kopyala"), move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(format!("smoothyfiles {ver}")))),
        ),
        update_card(p, &s),
        card(
            p,
            L::Download,
            t!("Güncellemeleri kendiliğinden indir"),
            Some(t!("Arka planda denetlenir ve indirilir; bir sonraki açılışta uygulanır.")),
            sw("auto-upd", p, s.auto_update, |s, v| s.auto_update = v),
        ),
        section(p, "GitHub"),
        link_row(p, "gh-star", L::Star, t!("GitHub'da yıldız ver"), Some(t!("smoothyfiles işinize yarıyorsa başkalarının da bulmasına yardım eder.")), |_, _| open_url(REPO_URL)),
        link_row(p, "coffee", L::Coffee, t!("Bana bir kahve ısmarla"), Some(t!("smoothyfiles'ı tek başıma geliştiriyorum; her destek, devam etmeme yardım eder.")), |_, _| open_url(crate::installer::DONATE)),
        link_row(p, "gh-issue", L::MessageSquare, t!("Hata bildir ya da öneride bulun"), None, |_, _| open_url(ISSUES_URL)),
        link_row(p, "gh-notes", L::ScrollText, t!("Sürüm notları"), None, |_, _| open_url(RELEASES_URL)),
        section(p, t!("Topluluk")),
        link_row(p, "site", L::Globe, t!("Web sitesi"), Some("latief.id/smoothyfiles"), |_, _| open_url(SITE_URL)),
        link_row(p, "discord", L::Users, t!("Discord sunucusu"), Some(t!("Soru sor, fikir paylaş, yeni sürümlerden haberdar ol.")), |_, _| open_url(DISCORD_URL)),
        div()
            .px_1()
            .text_xs()
            .text_color(p.text_secondary)
            .child(t!("Rust + GPUI ile yazılmış, hiç donmayan hafif dosya yöneticisi."))
            .into_any_element(),
        section(p, t!("Yardım ve destek")),
        link_row(p, "open-log", L::FolderOpen, t!("Kayıt ve ayar klasörünü aç"), settings::app_dir().map(|d| d.display().to_string()).as_deref(), |_, _| {
            if let Some(d) = settings::app_dir() {
                std::thread::spawn(move || {
                    let _ = std::process::Command::new("explorer.exe").arg(d).spawn();
                });
            }
        }),
        section(p, t!("Açık kaynak")),
        expander(
            "libs",
            p,
            L::Library,
            t!("Üçüncü taraf kütüphaneler"),
            None,
            None,
            move || {
                [
                    ("GPUI", "Apache-2.0"),
                    ("gpui-kit", "Apache-2.0"),
                    ("Velopack", "MIT"),
                    ("windows-rs", "MIT / Apache-2.0"),
                    ("Lucide icons", "ISC"),
                    ("zip, sevenz-rust2, tar, flate2, zstd", "MIT / Apache-2.0"),
                    ("notify", "CC0 / Artistic-2.0"),
                    ("chrono, serde, toml", "MIT / Apache-2.0"),
                ]
                .iter()
                .map(|(n, l)| sub(&pp, n, Some(l), div().into_any_element()))
                .collect()
            },
            cx,
        ),
    ]
}

/// Shows or hides a sidebar section (`Settings::sidebar_hidden`).
fn show_section(s: &mut Settings, key: &str, show: bool) {
    s.sidebar_hidden.retain(|h| h != key);
    if !show {
        s.sidebar_hidden.push(key.to_string());
    }
}

enum PickTarget {
    BackgroundImage,
    Ide,
    Export,
    Import,
}

/// Runs a Windows file dialog on the shell thread and applies the result.
fn pick_into(_window: &mut Window, cx: &mut App, title: &str, target: PickTarget) {
    let title = title.to_string();
    let (tx, rx) = futures::channel::oneshot::channel::<Option<std::path::PathBuf>>();
    let images = [(t!("Resimler"), "*.png;*.jpg;*.jpeg;*.webp;*.bmp;*.gif")];
    let exe = [(t!("Programlar"), "*.exe;*.cmd;*.bat")];
    let toml = [(t!("Ayar dosyası"), "*.toml")];
    let (images, exe, toml) = (
        images.map(|(a, b)| (a.to_string(), b.to_string())),
        exe.map(|(a, b)| (a.to_string(), b.to_string())),
        toml.map(|(a, b)| (a.to_string(), b.to_string())),
    );
    let kind = match target {
        PickTarget::BackgroundImage => 0,
        PickTarget::Ide => 1,
        PickTarget::Export => 2,
        PickTarget::Import => 3,
    };
    rf_win::sta::run(move |owner| {
        use rf_win::dialogs::{Pick, pick};
        fn f(v: &[(String, String)]) -> Vec<(&str, &str)> {
            v.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect()
        }
        let r = match kind {
            0 => pick(owner, &title, Pick::File { filters: &f(&images) }),
            1 => pick(owner, &title, Pick::File { filters: &f(&exe) }),
            2 => pick(owner, &title, Pick::Save { filters: &f(&toml), default_name: "smoothyfiles-ayarlar.toml" }),
            _ => pick(owner, &title, Pick::File { filters: &f(&toml) }),
        };
        let _ = tx.send(r);
    });
    cx.spawn(async move |cx| {
        let Ok(Some(path)) = rx.await else { return };
        let _ = cx.update(|cx| match kind {
            0 => settings::update(cx, |s| s.background_image = Some(path)),
            1 => settings::update(cx, |s| s.ide_path = path.display().to_string()),
            2 => {
                let s = cx.global::<Settings>().clone();
                std::thread::spawn(move || {
                    if let Ok(text) = toml::to_string_pretty(&s) {
                        let _ = std::fs::write(&path, text);
                    }
                });
            }
            _ => {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Ok(new) = Settings::parse_lenient_pub(text.trim_start_matches('\u{feff}')) {
                        // keep this machine's window placement and session
                        settings::update(cx, |s| {
                            let (window, tabs, active) = (s.window, s.session_tabs.clone(), s.session_active);
                            *s = new;
                            s.window = window;
                            s.session_tabs = tabs;
                            s.session_active = active;
                        });
                    }
                }
            }
        });
    })
    .detach();
}

/// "Local drives' file names are kept in an index … · C:\ 2.1 million entries, 84 MB · E:\ indexing…"
fn name_index_status() -> String {
    let mut s = t!("Yerel sürücülerdeki dosya adları bir dizinde tutulur: alt klasörlerde arama milisaniyeler sürer. Dizin arka planda hazırlanır ve güncel tutulur.").to_string();
    let parts: Vec<String> = rf_win::indexer::status()
        .into_iter()
        .map(|(root, state, live, mem)| {
            let r = root.to_string_lossy().trim_end_matches('\\').to_string();
            match state {
                rf_win::indexer::State::Building => format!("{r} {}", t!("hazırlanıyor…")),
                rf_win::indexer::State::Failed => format!("{r} {}", t!("dizinlenemedi")),
                rf_win::indexer::State::Ready => {
                    t!("{root} {n} öğe, {mb} MB").replace("{root}", &r).replace("{n}", &crate::bench_view::count(live as u64)).replace("{mb}", &(mem >> 20).to_string())
                }
            }
        })
        .collect();
    if !parts.is_empty() {
        s.push_str(" · ");
        s.push_str(&parts.join(" · "));
    }
    s
}

/// The folders left out of the search index: the user's (removable) and the built-in ones.
fn index_exclusions(p: &Palette, s: &Settings) -> AnyElement {
    let mut list = v_flex().py_1();
    for (i, dir) in s.index_excluded.iter().enumerate() {
        let d = dir.clone();
        list = list.child(
            h_flex()
                .min_h(px(40.))
                .pl(px(56.))
                .pr_4()
                .gap_3()
                .items_center()
                .child(Icon::new(L::FolderX).size(px(14.)).text_color(p.text_secondary))
                .child(div().flex_1().min_w_0().text_sm().text_ellipsis().overflow_hidden().whitespace_nowrap().child(dir.clone()))
                .child(
                    div()
                        .id(("idx-excl-rm", i))
                        .p_1()
                        .rounded(theme::RADIUS_SM)
                        .hover(|st| st.bg(p.pressed))
                        .child(Icon::new(L::X).size(px(14.)).text_color(p.text_secondary))
                        .on_click(move |_, _, cx| {
                            let d = d.clone();
                            settings::update(cx, |s| s.index_excluded.retain(|e| *e != d));
                        }),
                ),
        );
    }
    let defaults = rf_win::nameindex::default_excluded_names().join(", ");
    shell(p)
        .child(
            h_flex()
                .min_h(px(48.))
                .pl(px(56.))
                .pr_4()
                .py_2()
                .gap_4()
                .items_center()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().child(t!("Dizine alınmayan klasörler")))
                        .child(div().text_xs().text_color(p.text_secondary).child(format!(
                            "{} {defaults}, Windows. {}",
                            t!("Her zaman:"),
                            t!("Bu klasörlerin içinde arama diskte yapılır. Bir klasörü sağ tıklayıp \"Aramada dizine alma\" ile de ekleyebilirsiniz.")
                        ))),
                )
                .child(button("idx-excl-add", p, t!("Klasör ekle…"), |window, cx| {
                    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                    let owner = match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
                        Ok(RawWindowHandle::Win32(h)) => windows::Win32::Foundation::HWND(h.hwnd.get() as *mut _),
                        _ => windows::Win32::Foundation::HWND::default(),
                    };
                    if let Some(dir) = rf_win::dialogs::pick(owner, t!("Dizine alınmayacak klasör"), rf_win::dialogs::Pick::Folder) {
                        let d = dir.to_string_lossy().trim_end_matches('\\').to_string();
                        settings::update(cx, |s| {
                            if !s.index_excluded.iter().any(|e| e.eq_ignore_ascii_case(&d)) {
                                s.index_excluded.push(d);
                            }
                        });
                    }
                })),
        )
        .when(!s.index_excluded.is_empty(), |d| d.child(div().h(px(1.)).bg(p.divider)).child(list))
        .into_any_element()
}
