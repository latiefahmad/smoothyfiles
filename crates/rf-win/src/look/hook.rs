//! Sistem geneli klavye kancası: kendi iş parçacığında çalışır, Hızlı Bakış tuşlarını olaya çevirir.

use std::cell::RefCell;
use std::sync::RwLock;
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_RWIN, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, EVENT_SYSTEM_FOREGROUND, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN, MSG,
    PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use super::keys::{KeyGate, LookAction, LookKey};
use super::target::{TargetKind, foreground_target, is_typing};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LookEvent {
    pub action: LookAction,
    pub target: TargetKind,
    pub hwnd: isize,
}

/// Önizleme penceremizin hwnd'leri: bunlar öndeyken de tuşlar geçerlidir.
static OWN_WINDOWS: RwLock<Vec<isize>> = RwLock::new(Vec::new());

pub fn set_own_windows(hwnds: Vec<isize>) {
    *OWN_WINDOWS.write().unwrap_or_else(|e| e.into_inner()) = hwnds;
}

fn is_own(h: isize) -> bool {
    h != 0 && OWN_WINDOWS.read().unwrap_or_else(|e| e.into_inner()).contains(&h)
}

struct HookState {
    gate: KeyGate,
    tx: Sender<LookEvent>,
    trace: Option<(u64, Duration)>,
}

thread_local! {
    static STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

fn modifiers_down(info: &KBDLLHOOKSTRUCT) -> bool {
    let down = |vk: u16| unsafe { GetAsyncKeyState(vk as i32) } < 0;
    (info.flags.0 & LLKHF_ALTDOWN.0) != 0 || down(VK_SHIFT.0) || down(VK_CONTROL.0) || down(VK_LWIN.0) || down(VK_RWIN.0)
}

fn handle_key(st: &mut HookState, msg: u32, info: &KBDLLHOOKSTRUCT) {
    let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
    if !down && !matches!(msg, WM_KEYUP | WM_SYSKEYUP) {
        return;
    }
    let vk = info.vkCode;
    let (target, hwnd, ok) = if LookKey::from_vk(vk).is_some() {
        let (kind, h) = foreground_target();
        let ok = (kind != TargetKind::None || is_own(h)) && !is_typing(h);
        (kind, h, ok)
    } else {
        (TargetKind::None, 0, false)
    };
    let mods = LookKey::from_vk(vk).is_some() && modifiers_down(info);
    if let Some(action) = st.gate.on_key(vk, down, mods, info.time as u64, ok) {
        let _ = st.tx.send(LookEvent { action, target, hwnd });
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let start = Instant::now();
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        STATE.with(|s| {
            if let Some(st) = s.borrow_mut().as_mut() {
                handle_key(st, wparam.0 as u32, info);
                if let Some((n, max)) = st.trace.as_mut() {
                    *n += 1;
                    *max = (*max).max(start.elapsed());
                }
            }
        });
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn foreground_proc(_: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
    STATE.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            st.gate.foreground_changed();
        }
    });
}

pub struct HookHandle {
    thread_id: u32,
    hook: Option<JoinHandle<()>>,
    dispatch: Option<JoinHandle<()>>,
}

impl HookHandle {
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(h) = self.hook.take() {
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            let _ = h.join();
        }
        // dağıtım iş parçacığı beklenmez: Explorer'a COM çağrısının içinde (Explorer donmuşsa uzun
        // süre) olabilir; gönderici düştüğü için kendiliğinden biter
        self.dispatch.take();
    }
}

impl Drop for HookHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Kancayı kendi iş parçacığında kurar. `on_event` ayrı bir iş parçacığında çağrılır (kanca hiç beklemez).
pub fn start(on_event: impl Fn(LookEvent) + Send + 'static) -> HookHandle {
    let (tx, rx) = channel::<LookEvent>();
    let dispatch = std::thread::Builder::new()
        .name("look-dispatch".into())
        .spawn(move || {
            for ev in rx {
                on_event(ev);
            }
        })
        .expect("look dispatch thread");
    let (ready_tx, ready_rx) = channel::<u32>();
    let hook = std::thread::Builder::new()
        .name("look-hook".into())
        .spawn(move || {
            let trace = std::env::var_os("SMOOTHYFILES_LOOK_TRACE").is_some().then_some((0u64, Duration::ZERO));
            STATE.with(|s| *s.borrow_mut() = Some(HookState { gate: KeyGate::new(), tx, trace }));
            let kb: Option<HHOOK> = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0) }.ok();
            let fg = unsafe {
                SetWinEventHook(
                    EVENT_SYSTEM_FOREGROUND,
                    EVENT_SYSTEM_FOREGROUND,
                    None,
                    Some(foreground_proc),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            };
            // mesaj kuyruğu oluşsun: durdurma iletisi (WM_QUIT) daha döngüye girmeden gelirse kaybolmasın
            let mut probe = MSG::default();
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PeekMessageW(&mut probe, None, 0, 0, windows::Win32::UI::WindowsAndMessaging::PM_NOREMOVE);
            }
            let _ = ready_tx.send(unsafe { GetCurrentThreadId() });
            let mut msg = MSG::default();
            while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            unsafe {
                if let Some(k) = kb {
                    let _ = UnhookWindowsHookEx(k);
                }
                if !fg.is_invalid() {
                    let _ = UnhookWinEvent(fg);
                }
            }
            // Gönderici burada düşer: dağıtım iş parçacığı biter.
            let st = STATE.with(|s| s.borrow_mut().take());
            if let Some((n, max)) = st.and_then(|s| s.trace) {
                eprintln!("look-hook: {n} olay, en uzun {:?}", max);
            }
        })
        .expect("look hook thread");
    let thread_id = ready_rx.recv().unwrap_or(0);
    HookHandle { thread_id, hook: Some(hook), dispatch: Some(dispatch) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_and_stop_quickly() {
        let t = std::time::Instant::now();
        let h = start(|_| {});
        h.stop();
        assert!(t.elapsed() < std::time::Duration::from_millis(500));
    }

    /// Kancanın tuş başına işi (ön plan sınıflandırma, yazma denetimi, süzgeç): 1000 vuruşun
    /// %99'u 1 ms altında. (Tek en kötü değer makine yükünde iş parçacığı kesintisini ölçer:
    /// tam test takımı paralel koşarken bir kez 35 ms görüldü.)
    #[test]
    fn hook_work_under_one_ms() {
        let (tx, _rx) = channel();
        let mut st = HookState { gate: KeyGate::new(), tx, trace: None };
        let mut times = Vec::with_capacity(1000);
        for i in 0..1000u32 {
            let info = KBDLLHOOKSTRUCT { vkCode: super::super::keys::VK_SPACE, time: i * 10, ..Default::default() };
            let t = Instant::now();
            handle_key(&mut st, if i % 2 == 0 { WM_KEYDOWN } else { WM_KEYUP }, &info);
            times.push(t.elapsed());
        }
        times.sort();
        let p99 = times[989];
        assert!(p99 < Duration::from_millis(1), "p99 {p99:?}, worst {:?}", times[999]);
    }
}
