//! Hızlı Bakış'ın uygulamaya bağlanması: komut satırı, uzantı süzgeci, sistem geneli kanca,
//! Explorer seçiminin izlenmesi.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::*;
use rf_win::look::hook::{HookHandle, LookEvent};
use rf_win::look::keys::LookAction;
use rf_win::look::target::TargetKind;

use super::{LookManager, ShowOpts};
use crate::look::config::LookSettings;

/// Tek örnek borusunda Hızlı Bakış isteğinin öneki.
const IPC_PREFIX: &str = "\u{1}look:";

/// `smoothyfiles --look <yol> [top,pin]`.
pub fn parse_look_args(args: &[&str]) -> Option<(PathBuf, ShowOpts)> {
    if args.first().copied() != Some("--look") {
        return None;
    }
    let path = args.get(1)?.trim().trim_matches('"');
    if path.is_empty() {
        return None;
    }
    Some((PathBuf::from(path), parse_opts(args.get(2).copied().unwrap_or(""))))
}

fn parse_opts(s: &str) -> ShowOpts {
    let mut o = ShowOpts::default();
    for w in s.split(',').map(|w| w.trim().to_ascii_lowercase()) {
        match w.as_str() {
            "top" | "topmost" => o.topmost = true,
            "pin" => o.pin = true,
            _ => {}
        }
    }
    o
}

/// Çalışan örneğe gönderilecek ileti.
pub fn ipc_message(path: &Path, o: ShowOpts) -> String {
    let opts = [(o.topmost, "top"), (o.pin, "pin")].iter().filter(|(on, _)| *on).map(|(_, n)| *n).collect::<Vec<_>>().join(",");
    format!("{IPC_PREFIX}{opts}|{}", path.display())
}

pub fn parse_ipc(msg: &str) -> Option<(PathBuf, ShowOpts)> {
    let rest = msg.strip_prefix(IPC_PREFIX)?;
    let (opts, path) = rest.split_once('|')?;
    Some((PathBuf::from(path), parse_opts(opts)))
}

/// Uzantı süzgeci: izin listesi kipinde yalnız listedekiler, değilse engel listesi dışındakiler.
pub struct LookFilter {
    allow_mode: bool,
    allow: Vec<String>,
    block: Vec<String>,
}

fn ext_list(s: &str) -> Vec<String> {
    s.split([';', ',', ' ']).map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase()).filter(|e| !e.is_empty()).collect()
}

impl LookFilter {
    pub fn new(allow_mode: bool, allowlist: &str, blocklist: &str) -> LookFilter {
        LookFilter { allow_mode, allow: ext_list(allowlist), block: ext_list(blocklist) }
    }

    pub fn from_settings(s: &LookSettings) -> LookFilter {
        LookFilter::new(s.look_ext_allow_mode, &s.look_ext_allowlist, &s.look_ext_blocklist)
    }

    pub fn allows(&self, name: &str) -> bool {
        let ext = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if self.allow_mode { self.allow.contains(&ext) } else { !self.block.contains(&ext) }
    }
}

/// Süzgeçten geçerse gösterir.
pub fn show_filtered(path: PathBuf, opts: ShowOpts, cx: &mut App) {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !LookFilter::from_settings(cx.global::<LookSettings>()).allows(&name) {
        return;
    }
    LookManager::show(path, opts, cx);
}

/// Sistem geneli Hızlı Bakış: kanca ve Explorer'dan açılan önizlemenin kaynağı.
#[derive(Default)]
pub struct LookSystem {
    hook: Option<HookHandle>,
    /// Son önizleme Explorer/masaüstünden açıldıysa (tür, pencere): seçim izlenir.
    source: Option<(TargetKind, isize)>,
    watching: bool,
}

impl Global for LookSystem {}

/// Kanca iş parçacığından GPUI'ye giden olay: eylem ve (gerekiyorsa) seçili dosya.
struct HookMsg {
    ev: LookEvent,
    path: Option<PathBuf>,
}

pub fn quicklook_running() -> bool {
    rf_win::look::win::process_running("QuickLook.exe")
}

/// Ayara göre kancayı başlatır ya da durdurur (açılışta ve ayar değişince).
pub fn sync(cx: &mut App) {
    let want = cx.global::<LookSettings>().look_system_wide;
    let force = cx.global::<LookSettings>().look_ignore_quicklook;
    let running = cx.try_global::<LookSystem>().is_some_and(|s| s.hook.is_some());
    if want && !running {
        if quicklook_running() && !force {
            log::info!("look: QuickLook çalışıyor, sistem geneli kısayol başlatılmadı");
            return;
        }
        start(cx);
    } else if !want && running {
        if let Some(h) = cx.default_global::<LookSystem>().hook.take() {
            h.stop();
        }
    }
}

fn start(cx: &mut App) {
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<HookMsg>();
    let hook = rf_win::look::hook::start(move |ev| {
        // önizleme penceremiz öndeyse tuşları pencere kendisi işler
        if ev.target == TargetKind::None {
            return;
        }
        rf_win::shell::ensure_com();
        let path = match ev.action {
            LookAction::Toggle | LookAction::SelectionMoved => rf_win::look::target::selected_path(ev.target, ev.hwnd),
            _ => None,
        };
        let _ = tx.unbounded_send(HookMsg { ev, path });
    });
    cx.default_global::<LookSystem>().hook = Some(hook);
    cx.spawn(async move |cx| {
        use futures::StreamExt;
        while let Some(m) = rx.next().await {
            cx.update(|cx| handle(m, cx));
        }
    })
    .detach();
}

/// Enter'da dosyayı biz mi açarız? Kanca tuşu yutmadığı için Explorer ve masaüstü seçili öğeyi
/// zaten açar: orada yalnız önizleme kapanır.
pub fn opens_on_enter(target: TargetKind) -> bool {
    !matches!(target, TargetKind::Explorer | TargetKind::Desktop)
}

fn handle(m: HookMsg, cx: &mut App) {
    let visible = LookManager::is_visible(cx);
    match m.ev.action {
        LookAction::Toggle => {
            if visible {
                LookManager::close(cx);
            } else if let Some(p) = m.path {
                cx.default_global::<LookSystem>().source = Some((m.ev.target, m.ev.hwnd));
                show_filtered(p, ShowOpts::default(), cx);
                watch_selection(cx);
            }
        }
        LookAction::SelectionMoved if visible => {
            if let Some(p) = m.path {
                LookManager::switch(p, cx);
            }
        }
        LookAction::RunAndClose if visible => {
            if opens_on_enter(m.ev.target) {
                LookManager::run_and_close(cx)
            } else {
                LookManager::close(cx)
            }
        }
        LookAction::Close if visible => LookManager::close(cx),
        LookAction::Reload if visible => LookManager::reload(cx),
        LookAction::Fullscreen if visible => LookManager::fullscreen(cx),
        _ => {}
    }
}

/// Önizleme smoothyfiles içinden açıldı: Explorer seçimi izlenmez.
pub fn app_source(cx: &mut App) {
    cx.default_global::<LookSystem>().source = None;
}

/// Önizleme açıkken Explorer'daki seçim 300 ms'de bir okunur (fareyle seçim değişikliği için).
fn watch_selection(cx: &mut App) {
    if std::mem::replace(&mut cx.default_global::<LookSystem>().watching, true) {
        return;
    }
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_millis(300)).await;
            let src = cx.update(|cx| LookManager::is_visible(cx).then(|| cx.default_global::<LookSystem>().source).flatten());
            let Some((kind, hwnd)) = src else { break };
            let path = cx
                .background_spawn(async move {
                    rf_win::shell::ensure_com();
                    rf_win::look::target::selected_path(kind, hwnd)
                })
                .await;
            if let Some(p) = path {
                cx.update(|cx| LookManager::switch(p, cx));
            }
        }
        cx.update(|cx| cx.default_global::<LookSystem>().watching = false);
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{LookFilter, parse_look_args};
    use crate::look::ShowOpts;

    #[test]
    fn look_cli_args() {
        assert_eq!(parse_look_args(&["--look", r"C:\a b\x.mp4", "top,pin"]), Some((PathBuf::from(r"C:\a b\x.mp4"), ShowOpts { topmost: true, pin: true })));
        assert_eq!(parse_look_args(&["--look", r"C:\x.png"]), Some((PathBuf::from(r"C:\x.png"), ShowOpts::default())));
        assert_eq!(parse_look_args(&[r"C:\x"]), None);
        assert_eq!(parse_look_args(&["--look"]), None);
    }

    /// Kanca tuşu yutmaz: Explorer/masaüstü Enter'ı kendisi açar; biz açarsak dosya iki kez açılır.
    #[test]
    fn enter_in_explorer_only_closes() {
        use rf_win::look::target::TargetKind;
        assert!(!super::opens_on_enter(TargetKind::Explorer));
        assert!(!super::opens_on_enter(TargetKind::Desktop));
        assert!(super::opens_on_enter(TargetKind::None));
    }

    #[test]
    fn ext_filter() {
        let s = LookFilter::new(false, "", ".insv");
        assert!(!s.allows("a.INSV"));
        assert!(s.allows("a.mp4"));
        let a = LookFilter::new(true, ".mp4;.png", "");
        assert!(a.allows("x.png") && !a.allows("x.txt"));
    }

    #[test]
    fn ipc_message_round_trip() {
        let m = super::ipc_message(&PathBuf::from(r"C:\a\b.jpg"), ShowOpts { topmost: true, pin: false });
        assert_eq!(super::parse_ipc(&m), Some((PathBuf::from(r"C:\a\b.jpg"), ShowOpts { topmost: true, pin: false })));
        assert_eq!(super::parse_ipc(r"C:\klasör"), None);
    }
}
