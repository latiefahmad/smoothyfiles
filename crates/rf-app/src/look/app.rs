//! DirectLook programının girişi: ayrı süreç, kendi tek örnek borusu, kendi bildirim alanı simgesi.
//! smoothyfiles'tan iletiler (`ipc::Msg`) ve isteğe bağlı sistem geneli klavye kancasıyla çalışır.
//! smoothyfiles'ın `settings.toml`'undan yalnız dil ve temayı okur, hiç yazmaz.

use gpui_kit::*;

use super::config::LookSettings;
use super::ipc::{Msg, ToApp, app_name};
use super::{LookManager, ShowOpts};

/// Açılış argümanları: `--msg <ileti>` (smoothyfiles), `--look <yol> [top,pin]` (komut satırı),
/// `--background` (Windows'la başlama, ön ısıtma).
pub fn parse_args(args: &[&str]) -> Msg {
    match args.first().copied() {
        Some("--msg") => args.get(1).and_then(|m| Msg::decode(m)).unwrap_or(Msg::Wake),
        Some("--look") => match super::system::parse_look_args(args) {
            Some((p, o)) => Msg::Show(p, o),
            None => Msg::Wake,
        },
        // yalnız bir yol: göster
        Some(p) if !p.starts_with("--") => Msg::Show(p.into(), ShowOpts::default()),
        _ => Msg::Wake,
    }
}

fn tray_labels() -> rf_win::tray::Labels {
    rf_win::tray::Labels {
        tooltip: "DirectLook".into(),
        open: crate::t!("DirectLook ayarları").into(),
        new_window: String::new(),
        settings: String::new(),
        quit: crate::t!("Çıkış").into(),
    }
}

/// Ayarlar sayfasını smoothyfiles'ta açar (çalışmıyorsa başlatır).
fn open_settings() {
    std::thread::spawn(|| {
        if rf_win::single_instance::send_to("smoothyfiles", &ToApp::OpenSettings.encode()) {
            return;
        }
        if let Ok(me) = std::env::current_exe() {
            let _ = std::process::Command::new(me.with_file_name("smoothyfiles.exe")).arg("--settings=directlook").spawn();
        }
    });
}

/// DirectLook's own icon: when its setting is on and smoothyfiles has none (smoothyfiles running in
/// the background has one, with the settings in its menu): one icon, not two.
fn tray_wanted(cx: &App) -> bool {
    cx.global::<LookSettings>().look_tray_icon && !cx.global::<crate::settings::Settings>().run_in_background
}

fn sync_tray(cx: &App) {
    if tray_wanted(cx) {
        rf_win::tray::show(tray_labels());
    } else {
        rf_win::tray::hide();
    }
}

fn handle(msg: Msg, cx: &mut App) {
    match msg {
        Msg::Show(p, o) => LookManager::show(p, o, cx),
        Msg::Toggle(p) => {
            if LookManager::is_visible(cx) {
                LookManager::close(cx);
            } else {
                super::system::app_source(cx);
                super::system::show_filtered(p, ShowOpts::default(), cx);
            }
        }
        Msg::Follow(p) => {
            if LookManager::is_visible(cx) {
                LookManager::switch(p, cx);
            }
        }
        Msg::Close => LookManager::close(cx),
        Msg::Reload => {
            cx.set_global(LookSettings::load());
            // smoothyfiles' settings too: language, theme, and whether it has a tray icon
            cx.set_global(crate::settings::Settings::load());
            sync_tray(cx);
        }
        Msg::Quit => {
            rf_win::tray::hide();
            cx.quit();
        }
        Msg::Wake => {}
    }
}

/// directlook.exe
pub fn main() {
    crate::install_panic_log();
    rf_win::shell::suppress_critical_error_dialogs();
    rf_win::thumbs::keep_gdiplus_alive();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).target(env_logger::Target::Stderr).init();
    crate::background::DIRECTLOOK_PROCESS.store(true, std::sync::atomic::Ordering::Relaxed);

    let raw: Vec<String> = std::env::args().skip(1).collect();
    let first = parse_args(&raw.iter().map(String::as_str).collect::<Vec<_>>());
    let _instance = match rf_win::single_instance::acquire_app(&app_name(), &first.encode()) {
        rf_win::single_instance::Role::Forwarded => return,
        rf_win::single_instance::Role::Primary(g) => g,
    };
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<Msg>();
    {
        let tx = tx.clone();
        rf_win::single_instance::serve_app(&app_name(), move |m| {
            if let Some(msg) = Msg::decode(&m) {
                let _ = tx.unbounded_send(msg);
            }
        });
    }
    let (tray_tx, mut tray_rx) = futures::channel::mpsc::unbounded::<rf_win::tray::TrayEvent>();
    rf_win::tray::set_handler(move |e| {
        let _ = tray_tx.unbounded_send(e);
    });

    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(move |cx| {
        gpui_kit::init(cx);
        // smoothyfiles'ın ayarları: yalnız dil, tema, tarih biçimi için okunur, asla yazılmaz
        cx.set_global(crate::settings::Settings::load());
        if !rf_win::encoding::user_locale_is_turkish() {
            rf_core::textload::set_locale_hint(None);
        }
        crate::settings::apply_component_theme(cx);
        cx.set_global(LookSettings::load());
        sync_tray(cx);
        // önizleme penceresi kapanınca DirectLook çalışmayı sürdürür (arka planda bekler)
        cx.set_quit_mode(gpui_kit::QuitMode::Explicit);
        cx.on_window_closed(|cx, _| {
            if !LookManager::is_visible(cx) {
                std::thread::spawn(|| {
                    let _ = rf_win::single_instance::send_to("smoothyfiles", &ToApp::LookClosed.encode());
                });
            }
        })
        .detach();
        super::system::sync(cx);
        cx.observe_global::<LookSettings>(|cx| {
            super::system::sync(cx);
            sync_tray(cx);
        })
        .detach();
        handle(first.clone(), cx);
        cx.spawn(async move |cx| {
            use futures::StreamExt;
            while let Some(m) = rx.next().await {
                cx.update(|cx| handle(m, cx));
            }
        })
        .detach();
        cx.spawn(async move |cx| {
            use futures::StreamExt;
            while let Some(e) = tray_rx.next().await {
                match e {
                    rf_win::tray::TrayEvent::Quit => cx.update(|cx| handle(Msg::Quit, cx)),
                    _ => open_settings(),
                }
            }
        })
        .detach();
        drop(tx);
    });
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::parse_args;
    use crate::look::ShowOpts;
    use crate::look::ipc::Msg;

    #[test]
    fn launch_arguments() {
        assert_eq!(parse_args(&["--background"]), Msg::Wake);
        assert_eq!(parse_args(&[]), Msg::Wake);
        let m = Msg::Toggle(PathBuf::from(r"C:\a.jpg"));
        assert_eq!(parse_args(&["--msg", &m.encode()]), m);
        assert_eq!(parse_args(&["--look", r"C:\x.mp4", "top"]), Msg::Show(PathBuf::from(r"C:\x.mp4"), ShowOpts { topmost: true, pin: false }));
        assert_eq!(parse_args(&[r"C:\y.png"]), Msg::Show(PathBuf::from(r"C:\y.png"), ShowOpts::default()));
    }
}
