//! Installing and updating through Velopack (see notes/design/2026-11-08-install-update-design.md):
//! the Setup.exe on GitHub Releases installs per user, updates come from the same Releases.
//! A copy that wasn't installed by Velopack (a dev build, scripts/install.ps1) runs without
//! updates.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use velopack::sources::GithubSource;
use velopack::{UpdateCheck, UpdateInfo, UpdateManager, VelopackApp};

pub const REPO: &str = "https://github.com/latiefahmad/smoothyfiles";
/// AppUserModelID of the installed app (Velopack names shortcuts "velopack.<pack id>").
pub const APP_ID: &str = "velopack.smoothyfiles";

/// First thing in `main`: Velopack's install / update / uninstall hooks run here and exit.
pub fn startup() {
    VelopackApp::build()
        .on_after_install_fast_callback(|_| {
            after_install_or_update();
            dialogs_on_by_default();
        })
        .on_after_update_fast_callback(|_| after_install_or_update())
        .on_before_uninstall_fast_callback(|_| before_uninstall())
        .run();
}

/// Registrations that are on follow this install: the default file manager, DirectLook's start
/// with Windows, and the Open/Save dialog DLL (a per-build copy).
fn after_install_or_update() {
    let Ok(me) = std::env::current_exe() else { return };
    if rf_win::default_fm::is_enabled(rf_win::default_fm::CLASSES) {
        let _ = rf_win::default_fm::enable(&me.display().to_string(), rf_win::default_fm::CLASSES);
    }
    if rf_win::dialogs::starts_with_windows_as("DirectLook") {
        let _ = rf_win::dialogs::set_start_with_windows_as("DirectLook", &me.with_file_name("directlook.exe"), &["--background"], true);
    }
    if rf_win::dialog_reg::is_enabled(rf_win::dialog_reg::CLASSES) {
        if let Some(dll) = crate::dialog_dll() {
            rf_win::dialog_reg::point_to(&dll, rf_win::dialog_reg::CLASSES);
        }
    }
}

/// A fresh install turns on smoothyfiles' Open/Save dialogs (per user, no admin rights; Settings →
/// Advanced turns them off). Updates don't, so someone who turned them off keeps them off.
fn dialogs_on_by_default() {
    let classes = rf_win::dialog_reg::CLASSES;
    if !rf_win::dialog_reg::is_enabled(classes) {
        if let Some(dll) = crate::dialog_dll() {
            let _ = rf_win::dialog_reg::enable(&dll, classes);
        }
    }
}

/// Everything smoothyfiles changed in Windows goes back (settings in %APPDATA% stay).
fn before_uninstall() {
    let classes = rf_win::default_fm::CLASSES;
    if rf_win::default_fm::is_enabled(classes) {
        let _ = rf_win::default_fm::disable(classes);
    }
    let _ = rf_win::dialog_reg::disable(rf_win::dialog_reg::CLASSES);
    let none = Path::new("");
    let _ = rf_win::dialogs::set_start_with_windows_as("DirectLook", none, &[], false);
    let _ = rf_win::dialogs::set_start_with_windows(none, false);
    // DirectLook holds its exe open
    rf_win::single_instance::send_to(&crate::look::ipc::app_name(), &crate::look::ipc::Msg::Quit.encode());
    std::thread::sleep(Duration::from_millis(500));
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// Not installed by Velopack: no updates.
    NotInstalled,
    Idle,
    Checking,
    UpToDate,
    /// Version and percent downloaded.
    Downloading(String, u8),
    /// Downloaded: applied on restart (or at the next start anyway).
    Ready(String),
    Failed(String),
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static PENDING: Mutex<Option<UpdateInfo>> = Mutex::new(None);

pub fn state() -> State {
    STATE.lock().unwrap().clone().unwrap_or(State::Idle)
}

fn set(s: State) {
    if matches!(s, State::Ready(_)) && !matches!(state(), State::Ready(_)) {
        *READY_SINCE.lock().unwrap() = Some(std::time::Instant::now());
    }
    *STATE.lock().unwrap() = Some(s);
}

static READY_SINCE: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// When the downloaded update became ready (the Status Center button pulses for a few seconds).
pub fn ready_since() -> Option<std::time::Instant> {
    *READY_SINCE.lock().unwrap()
}

/// A check or download is running (the About page redraws while this is true).
pub fn busy() -> bool {
    matches!(state(), State::Checking | State::Downloading(..))
}

/// `SMOOTHYFILES_FAKE_UPDATE=1` (test copies): check, download 0→100 % and "ready" without a network,
/// to see the About page's states.
fn fake_check() {
    use std::thread::sleep;
    set(State::Checking);
    sleep(Duration::from_millis(1500));
    for pct in (0..=100).step_by(4) {
        set(State::Downloading("9.9.9".into(), pct));
        sleep(Duration::from_millis(120));
    }
    set(State::Ready("9.9.9".into()));
}

fn manager() -> Option<UpdateManager> {
    UpdateManager::new(GithubSource::new(REPO, None, false), None, None).ok()
}

/// This build's version (Velopack's when installed, else the crate's). Read once.
/// "0.1.0 · b241ace · 08.11.2026 02:40": the version, the commit and when it was built.
pub fn version_full() -> String {
    let built = env!("SMOOTHYFILES_BUILT").parse::<i64>().ok().and_then(|s| chrono::DateTime::from_timestamp(s, 0));
    let when = built.map(|t| t.with_timezone(&chrono::Local).format("%d.%m.%Y %H:%M").to_string()).unwrap_or_default();
    format!("{} · {} · {when}", version(), env!("SMOOTHYFILES_COMMIT"))
}

pub fn version() -> String {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| manager().map(|m| m.get_current_version_as_string()).unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())).clone()
}

/// Looks for an update and downloads it. Blocking: call off the UI thread.
pub fn check() {
    if std::env::var_os("SMOOTHYFILES_FAKE_UPDATE").is_some() {
        return fake_check();
    }
    let Some(um) = manager() else {
        set(State::NotInstalled);
        return;
    };
    if let Some(ready) = um.get_update_pending_restart() {
        set(State::Ready(ready.Version));
        return;
    }
    set(State::Checking);
    match um.check_for_updates() {
        Ok(UpdateCheck::UpdateAvailable(info)) => {
            let v = info.TargetFullRelease.Version.clone();
            set(State::Downloading(v.clone(), 0));
            // Velopack reports 0–100 on a channel; mirror it into the state while it downloads
            let (tx, rx) = std::sync::mpsc::channel::<i16>();
            let pv = v.clone();
            let progress = std::thread::spawn(move || {
                for pct in rx {
                    set(State::Downloading(pv.clone(), pct.clamp(0, 100) as u8));
                }
            });
            let result = um.download_updates(&info, Some(tx));
            let _ = progress.join();
            match result {
                Ok(()) => {
                    *PENDING.lock().unwrap() = Some(*info);
                    set(State::Ready(v));
                }
                Err(e) => set(State::Failed(e.to_string())),
            }
        }
        Ok(_) => set(State::UpToDate),
        Err(e) => set(State::Failed(e.to_string())),
    }
}

/// Starts the background checks: soon after start, then every 2 hours (when auto-download is
/// on; the Check button works either way). Calls `changed` on the UI thread after each.
pub fn spawn_checks(cx: &mut gpui_kit::App, changed: impl Fn(&mut gpui_kit::App) + 'static) {
    // test copies don't look for updates, unless they fake one (SMOOTHYFILES_FAKE_UPDATE)
    let fake = std::env::var_os("SMOOTHYFILES_FAKE_UPDATE").is_some();
    if std::env::var_os("SMOOTHYFILES_CONFIG_DIR").is_some() && !fake {
        return;
    }
    cx.spawn(async move |cx| {
        let mut wait = Duration::from_secs(if fake { 2 } else { 30 });
        loop {
            cx.background_executor().timer(wait).await;
            // a file manager stays open for days: a long gap meant releases waited a day to arrive
            // (GitHub allows 60 unauthenticated calls an hour; this is one call every two hours)
            wait = Duration::from_secs(2 * 3600);
            let auto = cx.update(|cx| cx.global::<crate::settings::Settings>().auto_update);
            if !auto {
                continue;
            }
            let run = cx.background_executor().spawn(async { check() });
            redraw_while_busy(cx).await;
            run.await;
            cx.update(|cx| changed(cx));
        }
    })
    .detach();
}

/// Redraws the windows a few times a second while a check or download runs (progress on About).
pub async fn redraw_while_busy(cx: &mut gpui_kit::AsyncApp) {
    cx.background_executor().timer(Duration::from_millis(50)).await;
    while busy() {
        cx.update(|cx| cx.refresh_windows());
        cx.background_executor().timer(Duration::from_millis(150)).await;
    }
    cx.update(|cx| cx.refresh_windows());
}

/// Restarts into the downloaded update. DirectLook is asked to quit first (its exe is replaced).
pub fn restart_to_update() {
    let Some(um) = manager() else { return };
    let pending = PENDING.lock().unwrap().clone();
    rf_win::single_instance::send_to(&crate::look::ipc::app_name(), &crate::look::ipc::Msg::Quit.encode());
    std::thread::sleep(Duration::from_millis(300));
    let r = match pending {
        Some(info) => um.apply_updates_and_restart(&info),
        None => match um.get_update_pending_restart() {
            Some(asset) => um.apply_updates_and_restart(&asset),
            None => return,
        },
    };
    if let Err(e) = r {
        set(State::Failed(e.to_string()));
    }
}

/// The one-time "star it on GitHub" strip: after a week and 30 starts, until answered.
pub fn star_due(s: &crate::settings::Settings) -> bool {
    let now = chrono::Utc::now().timestamp_millis();
    !s.star_asked && s.first_start_ms > 0 && s.starts >= 30 && now - s.first_start_ms >= 7 * 24 * 3600 * 1000
}

/// Where a coffee is bought (the README's button).
pub const DONATE: &str = "https://saweria.co/latiefahmad";

const DAY_MS: i64 = 24 * 3600 * 1000;

/// The "buy me a coffee" strip: after two weeks and 40 starts, then again some weeks after each
/// answer, until turned off. Never on the same day as the star strip.
pub fn donate_due(s: &crate::settings::Settings) -> bool {
    let now = chrono::Utc::now().timestamp_millis();
    if s.donate_never || star_due(s) || s.first_start_ms == 0 || s.starts < 40 {
        return false;
    }
    let first = s.first_start_ms + 14 * DAY_MS;
    now >= first.max(s.donate_next_ms)
}

/// How long the strip stays away after an answer: longer after a coffee than after "later".
pub fn donate_answered(s: &mut crate::settings::Settings, donated: bool) {
    let now = chrono::Utc::now().timestamp_millis();
    s.donate_next_ms = now + if donated { 120 } else { 30 } * DAY_MS;
    s.donate_seen = s.donate_seen.saturating_add(1);
}

/// Counted at each start of the app (not for test copies or a dialog-only start).
pub fn count_start(cx: &mut gpui_kit::App) {
    if std::env::var_os("SMOOTHYFILES_CONFIG_DIR").is_some() {
        return;
    }
    let now = chrono::Utc::now().timestamp_millis();
    crate::settings::update(cx, |s| {
        if s.first_start_ms == 0 {
            s.first_start_ms = now;
        }
        s.starts = s.starts.saturating_add(1);
    });
}

#[cfg(test)]
mod tests {
    use super::{DAY_MS, donate_answered, donate_due, star_due};

    #[test]
    fn coffee_strip_now_and_then() {
        let now = chrono::Utc::now().timestamp_millis();
        let mut s = crate::settings::Settings::default();
        s.star_asked = true;
        s.first_start_ms = now - 15 * DAY_MS;
        s.starts = 39;
        assert!(!donate_due(&s)); // too few starts
        s.starts = 40;
        assert!(donate_due(&s));
        s.first_start_ms = now - 13 * DAY_MS;
        assert!(!donate_due(&s)); // not two weeks yet
        s.first_start_ms = now - 15 * DAY_MS;
        donate_answered(&mut s, false);
        assert!(!donate_due(&s) && s.donate_seen == 1); // "later": a month away
        s.donate_next_ms = now - 1;
        assert!(donate_due(&s));
        donate_answered(&mut s, true);
        assert!(s.donate_next_ms - now >= 119 * DAY_MS); // after a coffee: four months
        s.donate_next_ms = 0;
        s.donate_never = true;
        assert!(!donate_due(&s));
        // never together with the star strip
        let mut t = crate::settings::Settings::default();
        t.first_start_ms = now - 15 * DAY_MS;
        t.starts = 50;
        assert!(star_due(&t) && !donate_due(&t));
    }

    #[test]
    fn star_strip_waits_a_week_and_30_starts() {
        let now = chrono::Utc::now().timestamp_millis();
        let week = 7 * 24 * 3600 * 1000;
        let mut s = crate::settings::Settings::default();
        assert!(!star_due(&s)); // never started
        s.first_start_ms = now - week - 1;
        s.starts = 29;
        assert!(!star_due(&s));
        s.starts = 30;
        assert!(star_due(&s));
        s.first_start_ms = now - week + 60_000;
        assert!(!star_due(&s)); // not a week yet
        s.first_start_ms = now - week - 1;
        s.star_asked = true;
        assert!(!star_due(&s));
    }
}
