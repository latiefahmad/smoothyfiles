//! Win+E while smoothyfiles is the default file manager. Windows 11 25H2 and 26H2 start File
//! Explorer from Win+E directly and skip the setting smoothyfiles writes, so the running app
//! holds the key itself (a global hotkey of its own): a press opens a window, also from the
//! background (tray). When the process ends the key is released, and Windows' own Win+E answers
//! again.

use gpui_kit::*;

static REGISTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Holds Win+E when the app is the default file manager, and routes presses to a new window.
/// Call once at startup (the side copies never get here: see `hotkey_owner`).
pub fn init(cx: &mut App) {
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
    rf_win::hotkey::set_win_e_handler(move || {
        let _ = tx.unbounded_send(());
    });
    cx.spawn(async move |cx| {
        use futures::StreamExt;
        while rx.next().await.is_some() {
            // like the taskbar's "New window": the last window comes forward, or a new one opens
            cx.update(|cx| crate::open_main_window(Some(rf_core::Location::Home), None, None, cx));
        }
    })
    .detach();
    sync(crate::DEFAULT_FM.load(std::sync::atomic::Ordering::Relaxed));
}

/// Registers or releases Win+E to match the default-file-manager switch (Settings › Advanced).
pub fn sync(on: bool) {
    let want = on;
    let held = REGISTERED.swap(false, std::sync::atomic::Ordering::Relaxed);
    if want == held {
        return;
    }
    match rf_win::hotkey::set_win_e(want) {
        Ok(()) => REGISTERED.store(want, std::sync::atomic::Ordering::Relaxed),
        Err(e) => log::warn!("Win+E: {e:?}"),
    }
}
