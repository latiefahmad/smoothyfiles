//! smoothyfiles ve DirectLook'un ortak kodu; iki program da bu kütüphaneyi kullanır.

mod actions;
mod aliases;
mod background;
pub mod bench;
mod bench_view;
mod donate;
mod archive_view;
mod bulk_rename;
mod command_palette;
mod conflict_dialog;
mod ctx_menu;
mod disk_usage;
mod fluent_menu;
mod flyout;
mod layout_panel;
mod git_ui;
mod prompt;
mod hash_panel;
mod i18n;
mod i18n_en;
mod folder_view;
mod icons;
mod keymap_editor;
mod layouts;
pub mod look;
mod image_cache;
mod installer;
mod ops;
mod picker;
mod places;
mod settings;
mod shell_view;
mod select_box;
mod settings_view;
mod taskbar_preview;
mod shelf_view;
mod status_center;
mod tags;
mod undo;
mod preview;
mod properties_view;
mod quick_search;
mod recycle_view;
mod theme;
mod workspace;

use std::path::PathBuf;

use gpui_kit::component::TitleBar;
use gpui_kit::*;
use rf_core::Location;

use crate::workspace::Workspace;

/// Window placement from the last session (ignored if it is implausibly small).
pub(crate) fn restored_bounds(cx: &App) -> Option<WindowBounds> {
    let [x, y, w, h, max] = cx.global::<settings::Settings>().window?;
    if w < 500. || h < 350. {
        return None;
    }
    let b = Bounds { origin: point(px(x), px(y)), size: size(px(w), px(h)) };
    Some(if max > 0.5 { WindowBounds::Maximized(b) } else { WindowBounds::Windowed(b) })
}

/// Panics are written to %APPDATA%\smoothyfiles\crash.log (the app has no console in release).
pub(crate) fn install_panic_log() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(dir) = crate::settings::app_dir() {
            let _ = std::fs::create_dir_all(&dir);
            let bt = std::backtrace::Backtrace::force_capture();
            let line = format!(
                "[{}] {info}\n{bt}\n\n",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
            );
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("crash.log")) {
                let _ = f.write_all(line.as_bytes());
            }
        }
        prev(info);
    }));
}

/// Whether smoothyfiles is registered as the default file manager (HKCU).
pub static DEFAULT_FM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// smoothyfiles answers other apps' Open/Save dialogs (registry state, read off the UI thread).
pub static FILE_DIALOGS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The Open/Save dialog DLL to register: a copy of the one next to this exe, outside the
/// install folder (`%LocalAppData%\smoothyfiles\dialog\<build>\`). Apps keep the registered DLL
/// loaded, and an update can't replace an install folder with a loaded DLL in it. Next to the
/// copy, `smoothyfiles-exe.txt` tells the DLL which smoothyfiles to start. Older copies go once no
/// app has them loaded.
pub(crate) fn dialog_dll() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let src = exe.with_file_name(rf_win::dialog_reg::DLL_NAME);
    let meta = std::fs::metadata(&src).ok()?;
    let stamp = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    let root = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("smoothyfiles").join("dialog");
    let dir = root.join(format!("{}-{:x}-{:x}", env!("CARGO_PKG_VERSION"), meta.len(), stamp));
    let dst = dir.join(rf_win::dialog_reg::DLL_NAME);
    if !dst.exists() {
        std::fs::create_dir_all(&dir).ok()?;
        grant_app_packages(&dir);
        let part = dir.join("smoothyfiles_dialog.dll.part");
        std::fs::copy(&src, &part).ok()?;
        std::fs::rename(&part, &dst).ok()?;
    }
    let _ = std::fs::write(dir.join("smoothyfiles-exe.txt"), exe.display().to_string());
    if let Ok(rd) = std::fs::read_dir(&root) {
        for e in rd.flatten() {
            if e.path() != dir {
                let _ = std::fs::remove_dir_all(e.path()); // fails while an app has it loaded
            }
        }
    }
    Some(dst.display().to_string())
}

/// Packaged apps' file pickers run in PickerHost.exe, which can only load the registered DLL if
/// app packages may read it; LocalAppData gives them no access, and the failed load makes the
/// picker fail (Snipping Tool crashed on Save as). Read and execute for ALL APPLICATION PACKAGES
/// and ALL RESTRICTED APPLICATION PACKAGES, inherited by the files copied in after.
fn grant_app_packages(dir: &std::path::Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("icacls")
        .arg(dir)
        .args(["/grant", "*S-1-15-2-1:(OI)(CI)(RX)", "*S-1-15-2-2:(OI)(CI)(RX)"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

#[cfg(test)]
mod grant_tests {
    #[test]
    fn app_packages_can_read_the_dialog_folder() {
        let dir = std::env::temp_dir().join(format!("rf-grant-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        super::grant_app_packages(&dir);
        let file = dir.join("smoothyfiles_dialog.dll");
        std::fs::write(&file, b"x").unwrap();
        // the file copied in after inherits the grant; SDDL (SIDs, not localized names):
        // AC is ALL APPLICATION PACKAGES, S-1-15-2-2 ALL RESTRICTED APPLICATION PACKAGES
        let saved = std::env::temp_dir().join(format!("rf-grant-{}.acl", std::process::id()));
        std::process::Command::new("icacls").arg(&file).arg("/save").arg(&saved).output().unwrap();
        let raw = std::fs::read(&saved).unwrap();
        let words: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let sddl = String::from_utf16_lossy(&words);
        let _ = std::fs::remove_file(&saved);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(sddl.contains("(A;ID;0x1200a9;;;AC)"), "{sddl}");
        assert!(sddl.contains("(A;ID;0x1200a9;;;S-1-15-2-2)"), "{sddl}");
    }
}

/// Opens a smoothyfiles window. `start = None` restores the session (first window) or opens Home.
pub fn open_main_window(
    start: Option<Location>,
    ipc: Option<futures::channel::mpsc::UnboundedReceiver<String>>,
    bounds: Option<WindowBounds>,
    cx: &mut App,
) {
    let options = WindowOptions {
        window_bounds: Some(bounds.unwrap_or_else(|| WindowBounds::centered(size(px(1280.), px(800.)), cx))),
        window_min_size: Some(size(px(720.), px(420.))),
        window_background: WindowBackgroundAppearance::MicaBackdrop,
        ..TitleBar::window_options()
    };
    if let Err(e) = gpui_kit::open_window(options, cx, move |window, cx| cx.new(|cx| Workspace::new(start, ipc, window, cx))) {
        log::error!("open window: {e:#}");
    }
}

/// Quits now, or once running copy/move/delete operations finish (closing the window must not
/// cut a copy in half). A window opened meanwhile (second launch) cancels the quit.
pub(crate) fn quit_when_idle(cx: &mut App) {
    // (an app or file the quick search is still starting counts as running work)
    let running = |cx: &App| cx.try_global::<status_center::OpsGlobal>().map_or(0, |g| g.0.read(cx).running()) + quick_search::launching();
    if running(cx) == 0 {
        save_and_quit(cx);
        return;
    }
    cx.spawn(async move |cx| loop {
        cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
        let done = cx.update(|cx| {
            if workspace::any_main_window(cx) {
                return Some(false);
            }
            (running(cx) == 0).then_some(true)
        });
        match done {
            Some(true) => {
                cx.update(save_and_quit);
                break;
            }
            Some(false) => break,
            None => {}
        }
    })
    .detach();
}

/// The session/placement save on window close runs on a helper thread; write once more here,
/// synchronously, so quitting cannot cut it off (a small file, only at exit).
fn save_and_quit(cx: &mut App) {
    cx.global::<settings::Settings>().clone().save();
    // the search index keeps its place in the change journal (or its changes): the next start
    // then catches up instead of walking the drives; bounded, quitting must not hang on it
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        rf_win::indexer::stop();
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
    cx.quit();
}

/// `Path::is_dir` with a time limit (it can block for the SMB timeout on an offline share).
fn is_dir_bounded(p: PathBuf, limit: std::time::Duration) -> Option<bool> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(p.is_dir());
    });
    rx.recv_timeout(limit).ok()
}

/// A shell namespace argument resolved to a folder we list ourselves that is not on the file
/// system (see `rf_win::shellfolder::browsable`), within `limit`.
fn shell_place_bounded(arg: &str, limit: std::time::Duration) -> Option<rf_win::shellfolder::Place> {
    let (tx, rx) = std::sync::mpsc::channel();
    let arg = arg.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(rf_win::shellfolder::browsable(&arg));
    });
    rx.recv_timeout(limit).ok().flatten().filter(|p| p.fs_path.is_none())
}

/// smoothyfiles'ın girişi.
/// Start-up timing (`SMOOTHYFILES_TRACE_START=1`, logged at info): milliseconds since the process
/// was created, at the steps before the first window.
pub(crate) fn start_mark(step: &str) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("SMOOTHYFILES_TRACE_START").is_some()) {
        eprintln!("start: {step} at {} ms", rf_win::process_age_ms());
    }
}

pub fn main() {
    start_mark("main");
    // Velopack (installer / updater): first, it may run an install hook and exit
    installer::startup();
    // the id Velopack gives the Start menu / taskbar shortcuts: a pin survives updates
    rf_win::window::set_app_id(installer::APP_ID);
    install_panic_log();
    rf_win::shell::suppress_critical_error_dialogs();
    // before any shell icon/thumbnail work: see keep_gdiplus_alive (loader-lock deadlock)
    rf_win::thumbs::keep_gdiplus_alive();
    // Logs go to stderr; override with RUST_LOG. The hybrid renderer (gpui_windows) logs its
    // GPU/CPU switches at info level.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,gpui_windows=info"))
        .target(env_logger::Target::Stderr)
        .format_timestamp_millis()
        .init();
    start_mark("logging");

    // As default file manager we also receive shell namespaces (Control Panel = "::{…}") and
    // non-folders: hand those back to Explorer so nothing becomes unreachable.
    // The folder check is bounded: an offline network path must not keep the window from
    // appearing for the whole SMB timeout. Undecided after 2 s = treat as a (slow) folder.
    // `--settings=directlook`: DirectLook'un simge menüsünden "DirectLook ayarları"
    // `--settings=speed`: straight to Settings → Speed test
    let settings_arg = std::env::args().nth(1).and_then(|a| a.strip_prefix("--settings=").map(str::to_string));
    let open_look_settings = settings_arg.is_some();
    // `--pick <pipe>`: an app's Open/Save dialog (smoothyfiles_dialog.dll) asks for a file
    let pick = (std::env::args().nth(1).as_deref() == Some("--pick")).then(|| std::env::args().nth(2)).flatten();
    // `--bar-preview`: our taskbar, as an ordinary window (see taskbar_preview)
    let bar_preview = std::env::args().nth(1).as_deref() == Some("--bar-preview");
    // `--quick-search`: the quick search window, as its hotkey opens it (a shortcut of the
    // user's own can run this)
    let quick_search = std::env::args().nth(1).as_deref() == Some(quick_search::LAUNCH_ARG);
    if bar_preview {
        quick_search::disable_hotkey();
    }
    let arg = std::env::args().nth(1).map(|a| a.trim().trim_matches('"').to_string()).filter(|a| !a.is_empty());
    let arg = arg.filter(|_| !open_look_settings && pick.is_none() && !bar_preview && !quick_search);
    // jump list task "New window"
    let new_window = arg.as_deref() == Some(rf_win::jumplist::NEW_WINDOW_ARG);
    let arg = arg.filter(|_| !new_window);
    let mut start: Option<Location> = None;
    // Explorer's `/select,<file>`: its folder, with the file selected
    // (callers also write `/select, <file>`: the path then comes as the next argument)
    let all_args = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let mut select = {
        let lower = all_args.to_ascii_lowercase();
        (lower.starts_with("/select,") || lower.starts_with("-select,"))
            .then(|| all_args[8..].trim().trim_matches('"').to_string())
            // bounded like the folder check below: an offline share must not hold the window
            .filter(|file| {
                PathBuf::from(file)
                    .parent()
                    .is_some_and(|d| is_dir_bounded(d.to_path_buf(), std::time::Duration::from_secs(2)) != Some(false))
            })
    };
    let arg = if select.is_some() { None } else { arg };
    if let Some(file) = &select {
        let p = PathBuf::from(file);
        start = p.parent().map(|d| Location::Path(d.to_path_buf()));
        *workspace::START_SELECT.lock().unwrap() = p.file_name().map(|n| n.to_string_lossy().into_owned());
    }
    // desktop icons (This PC, Recycle Bin) arrive as shell namespaces: those we show ourselves
    if let Some(loc) = arg.as_deref().and_then(Location::from_shell_name) {
        start = Some(loc);
    } else if let Some(arg) = arg {
        let shell_namespace = arg.starts_with("::") || arg.to_ascii_lowercase().starts_with("shell:");
        // a namespace folder we can list ourselves (a phone, Network, a library…) opens here;
        // Control Panel and other applets (and anything that does not enumerate) stay with Explorer.
        // Bounded like the folder check: a device that hangs must not keep the window away.
        let shell_place = if shell_namespace { shell_place_bounded(&arg, std::time::Duration::from_secs(3)) } else { None };
        if let Some(place) = shell_place {
            start = Some(Location::Shell { parsing: place.parsing, name: place.name });
        } else {
            // a network server root (\\nas) isn't a "directory" to Rust, but we browse its shares
            let server = rf_win::network::server_root(std::path::Path::new(&arg)).is_some();
            let is_dir = if shell_namespace {
                Some(false)
            } else if server {
                Some(true)
            } else {
                is_dir_bounded(PathBuf::from(&arg), std::time::Duration::from_secs(2))
            };
            // a browsable archive (DirectLook's "smoothyfiles'ta aç") opens like a folder, here
            let archive = !shell_namespace && archive_view::is_browsable(std::path::Path::new(&arg));
            if is_dir == Some(false) && !archive {
                // Explorer opens folders through the verb we took over: what we just handed it may
                // come straight back. Never ping-pong; a path is then shown here (with its error).
                if !rf_win::default_fm::forwarded_recently(&arg) {
                    rf_win::default_fm::note_forward(&arg);
                    let _ = std::process::Command::new("explorer.exe").arg(&arg).spawn();
                    return;
                }
                if shell_namespace {
                    return;
                }
                // a file that came back (an archive type Windows browses but we don't, e.g. .cpio):
                // its folder, with the file selected
                let p = PathBuf::from(&arg);
                if let (Some(dir), Some(name)) = (p.parent().filter(|_| p.is_file()), p.file_name()) {
                    start = Some(Location::Path(dir.to_path_buf()));
                    *workspace::START_SELECT.lock().unwrap() = Some(name.to_string_lossy().into_owned());
                    select = Some(arg.clone());
                }
            }
            if start.is_none() {
                start = Some(Location::Path(PathBuf::from(arg)));
            }
        }
    }
    // Cache the registry state for the settings page (read once, off the hot path).
    std::thread::spawn(|| {
        // copies opened/copied out of archives days ago
        crate::archive_view::clean_temp();
        // default file manager whose registered exe vanished (moved / rebuilt elsewhere):
        // re-point it here, or Windows can't open any folder. Not from test instances.
        if std::env::var_os("SMOOTHYFILES_CONFIG_DIR").is_none() {
            if let Ok(me) = std::env::current_exe() {
                if rf_win::default_fm::heal(&me.display().to_string(), rf_win::default_fm::CLASSES) {
                    log::warn!("default file manager re-registered to {}", me.display());
                }
                if rf_win::default_fm::add_archives(&me.display().to_string(), rf_win::default_fm::CLASSES) {
                    log::warn!("default file manager: archive registration updated");
                }
            }
        }
        // Open/Save dialogs: a registration whose DLL moved is pointed here (not from tests)
        if std::env::var_os("SMOOTHYFILES_CONFIG_DIR").is_none() {
            if rf_win::dialog_reg::is_enabled(rf_win::dialog_reg::CLASSES) {
                if let Some(dll) = dialog_dll() {
                    if rf_win::dialog_reg::point_to(&dll, rf_win::dialog_reg::CLASSES) {
                        log::warn!("file dialogs now use {dll}");
                    }
                }
            }
        }
        FILE_DIALOGS.store(rf_win::dialog_reg::is_enabled(rf_win::dialog_reg::CLASSES), std::sync::atomic::Ordering::Relaxed);
        DEFAULT_FM.store(
            rf_win::default_fm::is_enabled(rf_win::default_fm::CLASSES),
            std::sync::atomic::Ordering::Relaxed,
        )
    });

    // "Show in folder" from other apps (SHOpenFolderAndSelectItems: Telegram, browsers) while we
    // are the default file manager: Windows opens the folder through our verb and names the
    // file afterwards, to a window it finds in its window list (see rf_win::shell_window). The
    // folder opens at once; the file, if one comes, is selected in it.
    const SHELL_SELECT_WAIT: std::time::Duration = std::time::Duration::from_secs(4);
    let shell_select = match &start {
        Some(Location::Path(dir)) if select.is_none() && std::env::var_os("SMOOTHYFILES_MULTI_INSTANCE").is_none() && rf_win::default_fm::is_enabled(rf_win::default_fm::CLASSES) => {
            let (tx, rx) = std::sync::mpsc::channel();
            let dir = dir.clone();
            std::thread::spawn(move || {
                let _ = tx.send(rf_win::shell_window::wait_for_select(&dir, SHELL_SELECT_WAIT));
            });
            Some(rx)
        }
        _ => None,
    };
    let shell_selected = move |rx: std::sync::mpsc::Receiver<Option<PathBuf>>| {
        rx.recv_timeout(SHELL_SELECT_WAIT + std::time::Duration::from_secs(1)).ok().flatten().map(|f| format!("{}{}", workspace::SELECT_HERE_PREFIX, f.display()))
    };

    // Single instance: a second launch opens its folder as a tab in the running window.
    let message = match &start {
        _ if open_look_settings => look::ipc::ToApp::OpenSettings.encode(),
        _ if pick.is_some() => format!("{}{}", picker::PICK_PREFIX, pick.as_deref().unwrap_or_default()),
        _ if new_window => rf_win::jumplist::NEW_WINDOW_ARG.to_string(),
        _ if quick_search => quick_search::LAUNCH_ARG.to_string(),
        _ if select.is_some() => format!("{}{}", workspace::SELECT_PREFIX, select.as_deref().unwrap_or_default()),
        Some(Location::Path(p)) => p.to_string_lossy().into_owned(),
        Some(Location::RecycleBin) => "shell:RecycleBinFolder".to_string(),
        Some(Location::Shell { parsing, .. }) => format!("{}{parsing}", workspace::SHELL_PREFIX),
        // a search link goes on to the running window as a search link
        Some(Location::Search { root, query }) => {
            let enc = |s: &str| s.replace('%', "%25").replace('&', "%26");
            format!("search-ms:query={}&crumb=location:{}", enc(query), enc(&root.to_string_lossy()))
        }
        _ => String::new(),
    };
    // SMOOTHYFILES_MULTI_INSTANCE=1 skips forwarding (automated tests run several copies).
    let multi = std::env::var_os("SMOOTHYFILES_MULTI_INSTANCE").is_some() || bar_preview;
    let _instance = if multi {
        None
    } else {
        match rf_win::single_instance::acquire(&message) {
            rf_win::single_instance::Role::Forwarded => {
                // the folder is on its way to the running window; the file to select follows
                if let Some(msg) = shell_select.and_then(shell_selected) {
                    rf_win::single_instance::send_to("smoothyfiles", &msg);
                }
                return;
            }
            rf_win::single_instance::Role::Primary(guard) => Some(guard),
        }
    };
    let (ipc_tx, ipc_rx) = futures::channel::mpsc::unbounded::<String>();
    if let Some(rx) = shell_select {
        let tx = ipc_tx.clone();
        std::thread::spawn(move || {
            if let Some(msg) = shell_selected(rx) {
                let _ = tx.unbounded_send(msg);
            }
        });
    }
    if !multi {
        rf_win::single_instance::serve(move |m| {
            let _ = ipc_tx.unbounded_send(m);
        });
    }

    start_mark("before the app");
    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(move |cx| {
        start_mark("app running");
        gpui_kit::init(cx);
        cx.set_global(settings::Settings::load());
        start_mark("settings loaded");
        rf_core::sort::set_turkish_collation(rf_win::encoding::user_locale_is_turkish());
        // eski kod sayfalı metinlerin tahmini: Türkçe olmayan sistemde Türkçe ipucu verilmez
        if !rf_win::encoding::user_locale_is_turkish() {
            rf_core::textload::set_locale_hint(None);
        }
        settings::apply_component_theme(cx);
        cx.set_global(tags::load());
        let ops = cx.new(status_center::Ops::new);
        // the Status Center flyout is its own Acrylic window, opened/closed with `open`
        cx.observe(&ops, |ops, cx| {
            status_center::sync_popup(&ops, cx);
            status_center::sync_taskbar(&ops, cx);
        })
        .detach();
        cx.set_global(status_center::OpsGlobal(ops));
        actions::bind(cx);
        actions::bind_user(cx);
        keymap_editor::init(cx);
        if let Some(page) = &settings_arg {
            settings_view::open_page(
                match page.as_str() {
                    "speed" => settings_view::Page::Speed,
                    "general" => settings_view::Page::General,
                    _ => settings_view::Page::Look,
                },
                cx,
            );
        }
        // copies and moves keep a crash-safe journal there (see rf_win::fastcopy)
        if let Some(d) = settings::app_dir() {
            rf_win::fastcopy::set_journal_dir(d.join("jobs"));
        }
        // folder sizes survive a restart (shown at once, measured again when asked for)
        if let Some(d) = settings::local_data_dir() {
            rf_win::foldersize::shared_cache().persist(d.join("foldersizes.bin"));
        }
        let bounds = restored_bounds(cx);
        match pick.clone() {
            // started only to answer a dialog: no main window
            Some(pipe) => picker::begin(pipe, cx),
            None if bar_preview => taskbar_preview::open(cx),
            // started for the quick search only: no main window (it quits when that closes,
            // unless running in the background)
            None if quick_search => quick_search::toggle(cx),
            None => {
                installer::count_start(cx);
                donate::refresh();
                start_mark("opening the window");
                open_main_window(if open_look_settings { Some(Location::Settings) } else { start }, None, bounds, cx);
                start_mark("window opened");
                // a copy or move cut off last time: ask to carry it on
                status_center::offer_resume(cx);
            }
        }
        // updates from GitHub Releases (installed copies only)
        installer::spawn_checks(cx, |cx| cx.refresh_windows());
        // DirectLook ayrı süreç: önceden başlatılır ki ilk Boşluk beklemesin (testlerde değil)
        if cx.global::<settings::Settings>().directlook_enabled && std::env::var_os("SMOOTHYFILES_MULTI_INSTANCE").is_none() && !bar_preview {
            look::ipc::send(look::ipc::Msg::Wake, true);
        }
        // "Safely remove" / eject: let go of a drive when Windows asks (see rf_win::removal)
        {
            let (tx, mut rx) = futures::channel::mpsc::unbounded::<(char, std::sync::mpsc::Sender<()>)>();
            rf_win::removal::start(move |letter, ack| {
                let _ = tx.unbounded_send((letter, ack));
            });
            cx.spawn(async move |cx| {
                use futures::StreamExt;
                while let Some((letter, ack)) = rx.next().await {
                    cx.update(|cx| workspace::release_drive(letter, cx));
                    let _ = ack.send(());
                }
            })
            .detach();
        }
        // Second launches arrive here and go to whichever window is still open.
        let mut ipc_rx = ipc_rx;
        cx.spawn(async move |cx| {
            use futures::StreamExt;
            while let Some(msg) = ipc_rx.next().await {
                cx.update(|cx| workspace::route_ipc(msg, cx));
            }
        })
        .detach();
        // The last window closing ends the app (GPUI does not on its own: the process would
        // linger invisibly and swallow every later launch through the single-instance pipe).
        // With "run in the background" on, the app stays in the notification area instead.
        background::init(cx);
        quick_search::init(cx);
        // GPUI would quit by itself when the last window closes (QuitMode::Default on Windows):
        // that cut running copies in half and makes background mode impossible. We decide.
        cx.set_quit_mode(gpui_kit::QuitMode::Explicit);
        cx.on_window_closed(|cx, _| {
            if !workspace::any_main_window(cx) {
                // menus / flyouts left behind go too
                for w in cx.windows() {
                    let _ = w.update(cx, |_, window, _| window.remove_window());
                }
                if cx.global::<settings::Settings>().run_in_background && !background::quitting() {
                    let s = cx.global::<settings::Settings>().clone();
                    std::thread::spawn(move || s.save());
                } else {
                    quit_when_idle(cx);
                }
            }
        })
        .detach();
        cx.activate(true);
    });
}
