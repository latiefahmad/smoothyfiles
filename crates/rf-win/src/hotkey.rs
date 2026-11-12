//! One global keyboard shortcut (RegisterHotKey) for the quick search: works whichever app is
//! in front. It lives on a thread of its own with a hidden window, because a hotkey belongs to
//! the thread that registered it and arrives as WM_HOTKEY in that thread's queue.

use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};

use parking_lot::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey, VkKeyScanW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG, PostMessageW, RegisterClassW, TranslateMessage, WINDOW_EX_STYLE,
    WM_APP, WM_HOTKEY, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::w;

const ID: i32 = 0x5251; // "RQ"
const WM_SET: u32 = WM_APP + 31;

/// Why a shortcut can't be the hotkey.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyError {
    /// Not a shortcut we can register (no modifier, unknown key).
    Invalid,
    /// Another app (or Windows) has it.
    Taken,
}

struct Hotkey {
    hwnd: isize,
    /// Commands for the hotkey thread: the shortcut to hold (None: none) and where to answer.
    pending: Mutex<Option<(Option<(HOT_KEY_MODIFIERS, u32)>, Sender<Result<(), HotkeyError>>)>>,
    on_press: Mutex<Option<Box<dyn Fn() + Send>>>,
}

fn hotkey() -> &'static Hotkey {
    static H: OnceLock<Hotkey> = OnceLock::new();
    H.get_or_init(|| {
        let (tx, rx) = channel::<isize>();
        std::thread::Builder::new()
            .name("rf-hotkey".into())
            .spawn(move || unsafe {
                let inst = GetModuleHandleW(None).unwrap_or_default();
                let class = w!("smoothyfiles.Hotkey");
                let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst.into(), lpszClassName: class, ..Default::default() };
                RegisterClassW(&wc);
                let hwnd = CreateWindowExW(WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0), class, w!(""), WS_POPUP, 0, 0, 0, 0, None, None, Some(inst.into()), None)
                    .map(|h| h.0 as isize)
                    .unwrap_or(0);
                let _ = tx.send(hwnd);
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            })
            .expect("spawn hotkey thread");
        Hotkey { hwnd: rx.recv().unwrap_or(0), pending: Mutex::new(None), on_press: Mutex::new(None) }
    })
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY if wp.0 as i32 == ID => {
            if let Some(f) = hotkey().on_press.lock().as_ref() {
                f();
            }
            LRESULT(0)
        }
        WM_SET => {
            if let Some((keys, reply)) = hotkey().pending.lock().take() {
                unsafe {
                    let _ = UnregisterHotKey(Some(hwnd), ID);
                }
                let r = match keys {
                    None => Ok(()),
                    Some((mods, vk)) => unsafe { RegisterHotKey(Some(hwnd), ID, mods | MOD_NOREPEAT, vk) }.map_err(|_| HotkeyError::Taken),
                };
                let _ = reply.send(r);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Where a press goes (called on the hotkey thread; the newest caller wins).
pub fn set_handler(f: impl Fn() + Send + 'static) {
    *hotkey().on_press.lock() = Some(Box::new(f));
}

/// Makes `keys` (GPUI's syntax, as the shortcut editor records it: "alt-space",
/// "ctrl-shift-k") the hotkey, or drops it (`None`). Waits for the answer: `Taken` when another
/// app holds the shortcut.
pub fn set(keys: Option<&str>) -> Result<(), HotkeyError> {
    let parsed = match keys {
        None => None,
        Some(k) => Some(parse(k).ok_or(HotkeyError::Invalid)?),
    };
    let h = hotkey();
    if h.hwnd == 0 {
        return Err(HotkeyError::Taken);
    }
    let (tx, rx) = channel();
    *h.pending.lock() = Some((parsed, tx));
    unsafe {
        let _ = PostMessageW(Some(HWND(h.hwnd as *mut _)), WM_SET, WPARAM(0), LPARAM(0));
    }
    rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap_or(Err(HotkeyError::Taken))
}

/// "alt-space" → (MOD_ALT, VK_SPACE). A modifier is required (a bare key would be taken from
/// every app).
pub fn parse(keys: &str) -> Option<(HOT_KEY_MODIFIERS, u32)> {
    let keys = keys.trim().to_ascii_lowercase();
    let (mods_part, key) = match keys.strip_suffix("--") {
        Some(m) => (m.to_string(), "-".to_string()),
        None => match keys.rsplit_once('-') {
            Some((m, k)) => (m.to_string(), k.to_string()),
            None => (String::new(), keys.clone()),
        },
    };
    let mut mods = HOT_KEY_MODIFIERS(0);
    for m in mods_part.split('-').filter(|m| !m.is_empty()) {
        mods |= match m {
            "ctrl" => MOD_CONTROL,
            "alt" => MOD_ALT,
            "shift" => MOD_SHIFT,
            "win" => MOD_WIN,
            _ => return None,
        };
    }
    if mods.0 == 0 {
        return None;
    }
    let vk = match key.as_str() {
        "space" => 0x20,
        "enter" => 0x0D,
        "tab" => 0x09,
        "backspace" => 0x08,
        "escape" => return None,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "insert" => 0x2D,
        "delete" => 0x2E,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        k if k.len() >= 2 && k.starts_with('f') && k[1..].parse::<u32>().is_ok_and(|n| (1..=24).contains(&n)) => 0x6F + k[1..].parse::<u32>().ok()?,
        k if k.chars().count() == 1 => {
            let c = k.chars().next()?;
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase() as u32
            } else {
                // a punctuation key: where it sits on the current keyboard layout
                let r = unsafe { VkKeyScanW(c as u16) };
                if r == -1 {
                    return None;
                }
                (r & 0xFF) as u32
            }
        }
        _ => return None,
    };
    Some((mods, vk))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shortcuts() {
        assert_eq!(parse("alt-space"), Some((MOD_ALT, 0x20)));
        assert_eq!(parse("ctrl-shift-k"), Some((MOD_CONTROL | MOD_SHIFT, 0x4B)));
        assert_eq!(parse("win-alt-f5"), Some((MOD_WIN | MOD_ALT, 0x74)));
        assert_eq!(parse("ctrl-1"), Some((MOD_CONTROL, 0x31)));
        // no modifier, or not a key
        assert_eq!(parse("space"), None);
        assert_eq!(parse("ctrl-nothing"), None);
        assert_eq!(parse("ctrl-escape"), None);
    }

    /// Registers a shortcut nobody uses, then takes it back (no key is pressed).
    #[test]
    fn registers_and_drops() {
        assert_eq!(set(Some("ctrl-alt-shift-f23")), Ok(()));
        assert_eq!(set(None), Ok(()));
        assert_eq!(set(Some("space")), Err(HotkeyError::Invalid));
    }
}
