//! Running in the background (tray icon after the last window closes) and notifications for
//! long operations — Files' "leave app running in the background" and its toasts.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui_kit::*;

use crate::status_center::{OpPhase, Ops};
use crate::t;

/// Set while "Quit" from the tray closes the windows (so closing them really exits).
static QUITTING: AtomicBool = AtomicBool::new(false);
static TRAY_ON: AtomicBool = AtomicBool::new(false);

/// Operations shorter than this never notify.
const NOTIFY_AFTER: Duration = Duration::from_secs(10);

pub fn quitting() -> bool {
    QUITTING.load(Ordering::Relaxed)
}

fn labels() -> rf_win::tray::Labels {
    rf_win::tray::Labels {
        tooltip: "smoothyfiles".into(),
        open: t!("smoothyfiles'ı aç").into(),
        new_window: t!("Yeni pencere").into(),
        settings: t!("Ayarlar").into(),
        quit: t!("Çıkış").into(),
    }
}

/// Bu süreç DirectLook: smoothyfiles'ın bildirim alanı simgesi gösterilmez (DirectLook'un kendisi var).
pub static DIRECTLOOK_PROCESS: AtomicBool = AtomicBool::new(false);

/// Settings changed: the tray icon is there exactly while background mode is on.
pub fn sync_tray(on: bool) {
    if DIRECTLOOK_PROCESS.load(Ordering::Relaxed) {
        return;
    }
    if TRAY_ON.swap(on, Ordering::Relaxed) == on {
        return;
    }
    if on {
        rf_win::tray::show(labels());
    } else {
        rf_win::tray::hide();
    }
    // One icon in the notification area: while ours is there, DirectLook leaves its own out
    // (it reads this setting when told to reload; see look::app::tray_wanted).
    crate::look::ipc::send(crate::look::ipc::Msg::Reload, false);
}

/// Routes tray clicks to the app (call once at startup).
pub fn init(cx: &mut App) {
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<rf_win::tray::TrayEvent>();
    rf_win::tray::set_handler(move |e| {
        let _ = tx.unbounded_send(e);
    });
    cx.spawn(async move |cx| {
        use futures::StreamExt;
        while let Some(e) = rx.next().await {
            cx.update(|cx| on_tray(e, cx));
        }
    })
    .detach();
}

fn on_tray(e: rf_win::tray::TrayEvent, cx: &mut App) {
    use rf_win::tray::TrayEvent;
    match e {
        TrayEvent::Open => {
            // bring an open window forward, or open one (restoring the last session)
            let first = cx.windows().into_iter().next().filter(|_| crate::workspace::any_main_window(cx));
            match first {
                Some(w) => {
                    let _ = w.update(cx, |_, window, _| window.activate_window());
                }
                None => crate::open_main_window(None, None, crate::restored_bounds(cx), cx),
            }
        }
        TrayEvent::NewWindow => crate::open_main_window(Some(rf_core::Location::Home), None, None, cx),
        TrayEvent::Settings => crate::open_main_window(Some(rf_core::Location::Settings), None, None, cx),
        TrayEvent::Quit => {
            QUITTING.store(true, Ordering::Relaxed);
            let windows = cx.windows();
            if !crate::workspace::any_main_window(cx) {
                crate::quit_when_idle(cx);
            }
            for w in windows {
                let _ = w.update(cx, |_, window, _| window.remove_window());
            }
        }
    }
}

/// An operation ended: notify when it was long and smoothyfiles is not in front.
pub fn op_finished(ops: &Entity<Ops>, id: u64, cx: &mut App) {
    if !cx.global::<crate::settings::Settings>().notify_long_ops {
        return;
    }
    let Some((title, phase, took)) = ops.read(cx).summary(id) else { return };
    if took < NOTIFY_AFTER || rf_win::window::app_in_front() {
        return;
    }
    let (head, body) = match phase {
        OpPhase::Done => (t!("İşlem tamamlandı"), title),
        OpPhase::Failed(e) => (t!("İşlem başarısız oldu"), format!("{title}\n{e}")),
        OpPhase::Cancelled | OpPhase::Running => return,
    };
    rf_win::tray::notify(head, &body);
}
