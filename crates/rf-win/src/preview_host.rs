//! Hosts Windows preview handlers (IPreviewHandler — what Explorer's preview pane uses for
//! Office documents, PDF, video/audio, fonts, …) in a borderless window owned by the app
//! window and kept over the preview pane.
//!
//! Everything runs on the shell STA thread: a slow handler (Office can take a second) never
//! blocks the UI thread, and an *owned* window (unlike a child window) does not attach the two
//! threads' input queues.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER, CoCreateInstance, STGM_READ};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::PropertiesSystem::{IInitializeWithFile, IInitializeWithStream};
use windows::Win32::UI::Shell::{
    ASSOCF_INIT_DEFAULTTOSTAR, ASSOCSTR_SHELLEXTENSION, AssocQueryStringW, IInitializeWithItem, IPreviewHandler,
    IPreviewHandlerVisuals, IShellItem, SHCreateItemFromParsingName, SHCreateStreamOnFileEx,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, HWND_TOP, RegisterClassW, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetWindowPos,
    ShowWindow, WINDOW_EX_STYLE, WNDCLASSW, WS_CLIPCHILDREN, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{GUID, Interface, PCWSTR, w};

use crate::wide::wide;

const PREVIEW_HANDLER_IID: &str = "{8895b1c6-b41f-4c1c-a562-0d564250836f}";

/// CLSID of the preview handler registered for `path`'s type, if any.
pub fn handler_for(path: &Path) -> Option<GUID> {
    let ext = path.extension()?.to_string_lossy().to_string();
    let ext = wide(format!(".{ext}"));
    let iid = wide(PREVIEW_HANDLER_IID);
    let mut buf = [0u16; 64];
    let mut len = buf.len() as u32;
    unsafe {
        AssocQueryStringW(
            ASSOCF_INIT_DEFAULTTOSTAR,
            ASSOCSTR_SHELLEXTENSION,
            PCWSTR(ext.as_ptr()),
            PCWSTR(iid.as_ptr()),
            Some(windows::core::PWSTR(buf.as_mut_ptr())),
            &mut len,
        )
        .ok()
        .ok()?;
    }
    let s = crate::wide::from_wide(&buf);
    let s = s.trim_matches(|c| c == '{' || c == '}');
    GUID::try_from(s).ok()
}

struct Host {
    hwnd: HWND,
    owner: isize,
    path: Option<PathBuf>,
    handler: Option<IPreviewHandler>,
    /// Our own media player (Media Foundation's MFPlay) drawing into the same window, for
    /// audio / video that have no preview handler.
    player: Option<Player>,
}

struct Player {
    path: PathBuf,
    mp: windows::Win32::Media::MediaFoundation::IMFPMediaPlayer,
    /// Play was asked for before the file finished opening: the timer starts it.
    pending_play: u32,
    /// The last play / pause asked for, and when. MFPlay applies both asynchronously: its own
    /// state still says the old thing for a moment, and a second press decided from it would
    /// repeat the first command instead of undoing it.
    asked: Option<(bool, std::time::Instant)>,
}

const PLAY_TIMER: usize = 1;
/// For this long after a play / pause, what was asked answers "is it playing" (see `Player::asked`).
const ASKED_HOLDS: std::time::Duration = std::time::Duration::from_millis(500);

/// What the video's own window tells the app: clicks on the picture land on that window, never
/// on the app's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaEvent {
    /// Playback was started or paused (by the app or by a click on the picture).
    Playing(bool),
    /// The picture was double-clicked.
    DoubleClick,
}

static MEDIA_EVENTS: std::sync::Mutex<Option<Box<dyn Fn(MediaEvent) + Send>>> = std::sync::Mutex::new(None);

/// Sends the player's events to `f` (called on the shell STA thread; the newest caller wins).
pub fn set_media_events(f: impl Fn(MediaEvent) + Send + 'static) {
    *MEDIA_EVENTS.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(f));
}

fn emit(e: MediaEvent) {
    if let Some(f) = MEDIA_EVENTS.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        f(e);
    }
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{KillTimer, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_PAINT, WM_SIZE, WM_TIMER};
    // the player, if any (never while HOST is borrowed: messages can arrive re-entrantly)
    let player = || HOST.with(|h| h.try_borrow().ok().and_then(|h| h.as_ref().and_then(|x| x.player.as_ref().map(|p| p.mp.clone()))));
    match msg {
        WM_TIMER if wp.0 == PLAY_TIMER => {
            // start playback once the file is open (MFPlay opens it asynchronously)
            let Some(mp) = player() else {
                unsafe {
                    let _ = KillTimer(Some(hwnd), PLAY_TIMER);
                }
                return LRESULT(0);
            };
            // paused meanwhile (a timer message already queued outlives KillTimer): nothing to start
            let wanted = HOST.with(|h| h.try_borrow().ok().and_then(|h| h.as_ref().and_then(|x| x.player.as_ref().map(|p| p.pending_play > 0)))).unwrap_or(true);
            if !wanted {
                unsafe {
                    let _ = KillTimer(Some(hwnd), PLAY_TIMER);
                }
                return LRESULT(0);
            }
            let ready = unsafe { mp.GetMediaItem() }.is_ok();
            let started = ready && {
                if muted_for_tests() {
                    unsafe {
                        let _ = mp.SetMute(true);
                    }
                }
                unsafe { mp.Play() }.is_ok()
            };
            let give_up = HOST.with(|h| {
                h.try_borrow_mut().ok().and_then(|mut h| {
                    let p = h.as_mut()?.player.as_mut()?;
                    p.pending_play = p.pending_play.saturating_sub(1);
                    Some(p.pending_play == 0)
                })
            });
            if started || give_up != Some(false) {
                HOST.with(|h| {
                    if let Ok(mut h) = h.try_borrow_mut() {
                        if let Some(p) = h.as_mut().and_then(|x| x.player.as_mut()) {
                            p.pending_play = 0;
                        }
                    }
                });
                unsafe {
                    let _ = KillTimer(Some(hwnd), PLAY_TIMER);
                }
            }
            LRESULT(0)
        }
        WM_SIZE | WM_PAINT => {
            if let Some(mp) = player() {
                unsafe {
                    let _ = mp.UpdateVideo();
                }
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        // clicking the video toggles play / pause, like most players; a double click (two
        // clicks: paused and resumed, or the other way round) is the app's to answer (fullscreen)
        WM_LBUTTONUP => {
            media_toggle();
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            emit(MediaEvent::DoubleClick);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Automated UI tests set this: a test must never make a sound.
fn muted_for_tests() -> bool {
    std::env::var_os("SMOOTHYFILES_MUTE").is_some()
}

/// Whether `path` is audio / video our own player tries (Media Foundation decides in the end).
pub fn is_playable(path: &Path) -> bool {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    matches!(
        ext.as_str(),
        "mp4" | "m4v" | "mov" | "mkv" | "webm" | "avi" | "wmv" | "mpg" | "mpeg" | "ts" | "m2ts" | "3gp" | "mp3" | "wav" | "m4a" | "aac" | "flac" | "wma" | "ogg" | "opus"
    )
}

/// Whether Windows has a decoder (hardware or software) for `codec` ("hevc", "h264", "av1",
/// "vp9"; FilmCraft's codec names). HEVC and AV1 come from Store extensions that may be
/// missing; an unknown codec answers false.
pub fn mf_decodes(codec: &str) -> bool {
    use windows::Win32::Media::MediaFoundation::{
        IMFActivate, MFMediaType_Video, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE,
        MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_AV1,
        MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_VP90,
    };
    let c = codec.to_ascii_lowercase();
    let sub = if c.contains("hevc") || c.contains("265") {
        MFVideoFormat_HEVC
    } else if c.contains("h264") || c.contains("avc") || c.contains("264") {
        MFVideoFormat_H264
    } else if c.contains("av1") || c.contains("av01") {
        MFVideoFormat_AV1
    } else if c.contains("vp9") || c.contains("vp09") {
        MFVideoFormat_VP90
    } else {
        return false;
    };
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: sub };
    // DirectLook may ask from a worker thread that has no apartment yet (already in one is fine)
    let _ = unsafe { windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED) };
    unsafe {
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut n = 0u32;
        let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER;
        if MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, flags, Some(&input), None, &mut list, &mut n).is_err() || list.is_null() {
            return false;
        }
        for i in 0..n as usize {
            drop(std::ptr::read(list.add(i)));
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(list as *const _));
        n > 0
    }
}

type MfpCreate = unsafe extern "system" fn(
    windows::core::PCWSTR,
    windows_core::BOOL,
    windows::Win32::Media::MediaFoundation::MFP_CREATION_OPTIONS,
    *mut core::ffi::c_void,
    HWND,
    *mut *mut core::ffi::c_void,
) -> windows_core::HRESULT;

/// Media Foundation is loaded on the first Play, not at start-up: linking MFPCreateMediaPlayer
/// statically made every launch load mfplay.dll and mfplat.dll. Also runs MFStartup once.
fn mf_create() -> Option<MfpCreate> {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::core::s;
    static F: std::sync::OnceLock<Option<MfpCreate>> = std::sync::OnceLock::new();
    *F.get_or_init(|| unsafe {
        let plat = LoadLibraryW(w!("mfplat.dll")).ok()?;
        if let Some(startup) = GetProcAddress(plat, s!("MFStartup")) {
            // SAFETY: MFStartup(ULONG Version, DWORD dwFlags) -> HRESULT
            let startup: unsafe extern "system" fn(u32, u32) -> windows_core::HRESULT = std::mem::transmute(startup);
            let _ = startup(windows::Win32::Media::MediaFoundation::MF_VERSION, windows::Win32::Media::MediaFoundation::MFSTARTUP_FULL);
        }
        let play = LoadLibraryW(w!("mfplay.dll")).ok()?;
        let f = GetProcAddress(play, s!("MFPCreateMediaPlayer"))?;
        // SAFETY: the documented signature (see MfpCreate)
        Some(std::mem::transmute::<unsafe extern "system" fn() -> isize, MfpCreate>(f))
    })
}

/// Plays `path` with our own player; `rect` (screen pixels) shows its video over the pane,
/// `None` plays without a visible window (audio). Re-pressing play on the same file resumes.
/// Call on the shell STA thread.
pub fn media_play(owner: isize, path: &Path, rect: Option<RECT>) {
    use windows::Win32::Media::MediaFoundation::{IMFPMediaPlayer, MFP_OPTION_NONE};
    use windows::Win32::UI::WindowsAndMessaging::SetTimer;
    use windows_core::Interface;
    let Some(hwnd) = host_window(owner) else { return };
    // a shell preview handler shares the window: unload it
    let old = HOST.with(|h| h.borrow_mut().as_mut().and_then(|x| {
        x.path = None;
        x.handler.take()
    }));
    unload(old);
    match rect {
        Some(r) => position(hwnd, r),
        None => unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        },
    }
    let same = HOST.with(|h| h.borrow().as_ref().is_some_and(|x| x.player.as_ref().is_some_and(|p| p.path == path)));
    if !same {
        stop_player();
        let Some(create) = mf_create() else {
            log::info!("media player: Media Foundation unavailable");
            return;
        };
        let w = wide(path);
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: the documented MFPCreateMediaPlayer signature; `raw` receives an owned reference
        let hr = unsafe { create(PCWSTR(w.as_ptr()), windows_core::BOOL(0), MFP_OPTION_NONE, std::ptr::null_mut(), hwnd, &mut raw) };
        if hr.is_err() || raw.is_null() {
            log::info!("media player: can't open {} ({hr:?})", path.display());
            return;
        }
        // SAFETY: a non-null IMFPMediaPlayer pointer whose reference we now own
        let mp = unsafe { IMFPMediaPlayer::from_raw(raw) };
        HOST.with(|h| {
            if let Some(x) = h.borrow_mut().as_mut() {
                x.player = Some(Player { path: path.to_path_buf(), mp, pending_play: 0, asked: None });
            }
        });
    }
    // ~6 s for the file to open; the timer plays it as soon as it can
    HOST.with(|h| {
        if let Some(p) = h.borrow_mut().as_mut().and_then(|x| x.player.as_mut()) {
            p.pending_play = 200;
            p.asked = Some((true, std::time::Instant::now()));
        }
    });
    unsafe {
        let _ = SetTimer(Some(hwnd), PLAY_TIMER, 30, None);
    }
}

fn current_player() -> Option<windows::Win32::Media::MediaFoundation::IMFPMediaPlayer> {
    HOST.with(|h| h.try_borrow().ok().and_then(|h| h.as_ref().and_then(|x| x.player.as_ref().map(|p| p.mp.clone()))))
}

/// Whether the player plays, as far as the user is concerned: a play still waiting for the file
/// to open counts, and for a moment after a play / pause what was asked does (see `Player::asked`).
fn playing_now(mp: &windows::Win32::Media::MediaFoundation::IMFPMediaPlayer) -> bool {
    use windows::Win32::Media::MediaFoundation::MFP_MEDIAPLAYER_STATE_PLAYING;
    let (pending, asked) =
        HOST.with(|h| h.try_borrow().ok().and_then(|h| h.as_ref().and_then(|x| x.player.as_ref().map(|p| (p.pending_play > 0, p.asked))))).unwrap_or((false, None));
    match asked {
        Some((want, at)) if at.elapsed() < ASKED_HOLDS => want,
        _ => pending || unsafe { mp.GetState() }.is_ok_and(|s| s == MFP_MEDIAPLAYER_STATE_PLAYING),
    }
}

/// Plays (`Some(true)`), pauses (`Some(false)`) or toggles (`None`) and returns whether the
/// player now plays; `None` without a player. The one place play / pause is decided, on the
/// thread that owns the player: the app's button and a click on the picture both end here, so
/// neither works from a stale copy of the state. The video window is left as it is.
fn set_playing(want: Option<bool>) -> Option<bool> {
    use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};
    let mp = current_player()?;
    let play = want.unwrap_or_else(|| !playing_now(&mp));
    let ready = unsafe { mp.GetMediaItem() }.is_ok();
    // (never call the player while HOST is borrowed: its calls can re-enter the window procedure)
    let hwnd = HOST.with(|h| {
        let mut h = h.try_borrow_mut().ok()?;
        let x = h.as_mut()?;
        let p = x.player.as_mut()?;
        p.asked = Some((play, std::time::Instant::now()));
        // a pause also calls off a play still waiting for the file to open (the timer would
        // start it a moment later and the pause would be lost); a play before the file is open
        // waits for it the same way the first one does
        p.pending_play = if play && !ready { 200 } else { 0 };
        Some(x.hwnd)
    })?;
    unsafe {
        if play && ready {
            if muted_for_tests() {
                let _ = mp.SetMute(true);
            }
            let _ = mp.Play();
        } else if play {
            let _ = SetTimer(Some(hwnd), PLAY_TIMER, 30, None);
        } else {
            let _ = KillTimer(Some(hwnd), PLAY_TIMER);
            let _ = mp.Pause();
        }
    }
    emit(MediaEvent::Playing(play));
    Some(play)
}

/// Pauses our player. STA thread.
pub fn media_pause() {
    set_playing(Some(false));
}

/// Plays if paused, pauses if playing; returns whether it now plays. STA thread.
pub fn media_toggle() -> Option<bool> {
    set_playing(None)
}

/// Jumps to `fraction` (0..1) of the length. STA thread.
pub fn media_seek(fraction: f64) {
    use windows::Win32::Media::MediaFoundation::MFP_POSITIONTYPE_100NS;
    let Some(mp) = current_player() else { return };
    unsafe {
        let Ok(d) = mp.GetDuration(&MFP_POSITIONTYPE_100NS) else { return };
        let Ok(total) = i64::try_from(&d) else { return };
        let at = windows::Win32::System::Com::StructuredStorage::PROPVARIANT::from((total as f64 * fraction.clamp(0.0, 1.0)) as i64);
        let _ = mp.SetPosition(&MFP_POSITIONTYPE_100NS, &at);
    }
}

/// Playback state for the controls.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MediaState {
    pub playing: bool,
    /// Seconds.
    pub position: f64,
    pub duration: f64,
}

/// The player's state, `None` without a player. STA thread.
pub fn media_state() -> Option<MediaState> {
    use windows::Win32::Media::MediaFoundation::MFP_POSITIONTYPE_100NS;
    let mp = current_player()?;
    unsafe {
        let secs = |v: windows::core::Result<windows::Win32::System::Com::StructuredStorage::PROPVARIANT>| v.ok().and_then(|v| i64::try_from(&v).ok()).map_or(0.0, |t| t as f64 / 1e7);
        Some(MediaState {
            // a play that is about to start counts as playing (the button doesn't flicker)
            playing: playing_now(&mp),
            position: secs(mp.GetPosition(&MFP_POSITIONTYPE_100NS)),
            duration: secs(mp.GetDuration(&MFP_POSITIONTYPE_100NS)),
        })
    }
}

/// Moves the player's video (pane resized / window moved). STA thread.
pub fn media_reposition(rect: RECT) {
    let hwnd = HOST.with(|h| h.borrow().as_ref().filter(|x| x.player.is_some()).map(|x| x.hwnd));
    if let Some(hwnd) = hwnd {
        position(hwnd, rect);
        if let Some(mp) = current_player() {
            unsafe {
                let _ = mp.UpdateVideo();
            }
        }
    }
}

/// Stops and releases our player (the file is closed).
fn stop_player() {
    let p = HOST.with(|h| h.borrow_mut().as_mut().and_then(|x| x.player.take()));
    if let Some(p) = p {
        unsafe {
            let _ = p.mp.Shutdown();
        }
    }
}

fn host_window(owner: isize) -> Option<HWND> {
    HOST.with(|h| {
        let mut h = h.borrow_mut();
        if let Some(host) = h.as_ref() {
            if host.owner == owner {
                return Some(host.hwnd);
            }
        }
        // a different app window owns the pane now: start over
        if let Some(old) = h.take() {
            unload(old.handler);
            if let Some(p) = old.player {
                unsafe {
                    let _ = p.mp.Shutdown();
                }
            }
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(old.hwnd);
            }
        }
        unsafe {
            let inst = GetModuleHandleW(None).ok()?;
            let class = w!("smoothyfiles-preview-host");
            // CS_DBLCLKS: the picture's double click arrives as one (see WM_LBUTTONDBLCLK)
            let wc = WNDCLASSW { style: windows::Win32::UI::WindowsAndMessaging::CS_DBLCLKS, lpfnWndProc: Some(wndproc), hInstance: inst.into(), lpszClassName: class, ..Default::default() };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0),
                class,
                w!(""),
                WS_POPUP | WS_CLIPCHILDREN,
                0,
                0,
                10,
                10,
                Some(HWND(owner as *mut _)),
                None,
                Some(inst.into()),
                None,
            )
            .ok()?;
            *h = Some(Host { hwnd, owner, path: None, handler: None, player: None });
            Some(hwnd)
        }
    })
}

fn unload(handler: Option<IPreviewHandler>) {
    if let Some(ph) = handler {
        unsafe {
            let _ = ph.Unload();
        }
    }
}

/// Theme colors for handlers that support it (dark mode text/background).
pub struct Colors {
    pub background: u32,
    pub text: u32,
}

/// Shows `path` over `rect` (screen pixels). Returns false if no handler could preview it.
/// Call on the shell STA thread.
pub fn show(owner: isize, path: &Path, rect: RECT, colors: &Colors) -> bool {
    let Some(hwnd) = host_window(owner) else { return false };
    stop_player();
    let same = HOST.with(|h| h.borrow().as_ref().is_some_and(|x| x.path.as_deref() == Some(path) && x.handler.is_some()));
    position(hwnd, rect);
    if same {
        HOST.with(|h| {
            if let Some(ph) = h.borrow().as_ref().and_then(|x| x.handler.clone()) {
                let r = client_rect(rect);
                unsafe {
                    let _ = ph.SetRect(&r);
                }
            }
        });
        return true;
    }
    // replace the previous preview
    let old = HOST.with(|h| h.borrow_mut().as_mut().and_then(|x| {
        x.path = None;
        x.handler.take()
    }));
    unload(old);
    match load(hwnd, path, rect, colors) {
        Some(ph) => {
            HOST.with(|h| {
                if let Some(x) = h.borrow_mut().as_mut() {
                    x.path = Some(path.to_path_buf());
                    x.handler = Some(ph);
                }
            });
            true
        }
        None => {
            hide();
            false
        }
    }
}

fn client_rect(rect: RECT) -> RECT {
    RECT { left: 0, top: 0, right: rect.right - rect.left, bottom: rect.bottom - rect.top }
}

fn position(hwnd: HWND, rect: RECT) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            rect.left,
            rect.top,
            (rect.right - rect.left).max(1),
            (rect.bottom - rect.top).max(1),
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

fn load(hwnd: HWND, path: &Path, rect: RECT, colors: &Colors) -> Option<IPreviewHandler> {
    let clsid = handler_for(path)?;
    unsafe {
        let ph: IPreviewHandler = CoCreateInstance(&clsid, None, CLSCTX_INPROC_SERVER | CLSCTX_LOCAL_SERVER).ok()?;
        let wpath = wide(path);
        let mut ok = false;
        if let Ok(init) = ph.cast::<IInitializeWithItem>() {
            if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wpath.as_ptr()), None) {
                ok = init.Initialize(&item, STGM_READ.0).is_ok();
            }
        }
        if !ok {
            if let Ok(init) = ph.cast::<IInitializeWithFile>() {
                ok = init.Initialize(PCWSTR(wpath.as_ptr()), STGM_READ.0).is_ok();
            }
        }
        if !ok {
            if let Ok(init) = ph.cast::<IInitializeWithStream>() {
                if let Ok(stream) = SHCreateStreamOnFileEx(PCWSTR(wpath.as_ptr()), STGM_READ.0, 0, false, None) {
                    ok = init.Initialize(&stream, STGM_READ.0).is_ok();
                }
            }
        }
        if !ok {
            return None;
        }
        if let Ok(vis) = ph.cast::<IPreviewHandlerVisuals>() {
            let _ = vis.SetBackgroundColor(COLORREF(bgr(colors.background)));
            let _ = vis.SetTextColor(COLORREF(bgr(colors.text)));
        }
        let r = client_rect(rect);
        ph.SetWindow(hwnd, &r).ok()?;
        ph.DoPreview().ok()?;
        Some(ph)
    }
}

/// 0xRRGGBB → COLORREF 0x00BBGGRR
fn bgr(c: u32) -> u32 {
    ((c & 0xFF) << 16) | (c & 0xFF00) | ((c >> 16) & 0xFF)
}

/// Hides the host and unloads the handler (frees the document). Call on the STA thread.
pub fn hide() {
    stop_player();
    let (hwnd, handler) = HOST.with(|h| match h.borrow_mut().as_mut() {
        Some(x) => {
            x.path = None;
            (Some(x.hwnd), x.handler.take())
        }
        None => (None, None),
    });
    unload(handler);
    if let Some(hwnd) = hwnd {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// Moves the visible host (window moved / pane resized) without reloading.
pub fn reposition(rect: RECT) {
    HOST.with(|h| {
        if let Some(x) = h.borrow().as_ref() {
            if x.handler.is_some() {
                position(x.hwnd, rect);
                if let Some(ph) = &x.handler {
                    unsafe {
                        let _ = ph.SetRect(&client_rect(rect));
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod mf_tests {
    #[test]
    fn h264_has_a_decoder_and_unknown_codecs_dont() {
        // Windows always ships an H.264 decoder
        assert!(super::mf_decodes("h264"));
        assert!(!super::mf_decodes("prores"));
        // HEVC comes from a Store extension: print, don't assert
        eprintln!("hevc decoder: {}", super::mf_decodes("hevc"));
    }
}
