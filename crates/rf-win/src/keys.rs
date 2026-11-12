//! Keyboard-layout helpers. GPUI turns Shift+digit/punctuation into the layout's shifted
//! character and drops Shift (Turkish Q: Shift+2 → `'`, US: Shift+2 → `@`), so a binding written
//! as `ctrl-shift-2` never matches. `shifted_char` tells the app which key to bind instead.

use windows::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC, MapVirtualKeyW, ToUnicode, VK_SHIFT};

/// Character the current keyboard layout produces for `vk` with Shift held, if printable.
pub fn shifted_char(vk: u16) -> Option<String> {
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) };
    let mut state = [0u8; 256];
    state[VK_SHIFT.0 as usize] = 0x80;
    let mut buf = [0u16; 8];
    // same flags as GPUI's own lookup (0x1: menu active, 0x4: don't touch the keyboard state,
    // so dead keys stay pending for the real typing). A dead key (Turkish Shift+3 = ^)
    // returns a negative length but GPUI still reports its character, so do the same.
    let n = unsafe { ToUnicode(vk as u32, scan, Some(&state), &mut buf, 0x5) };
    let len = n.unsigned_abs() as usize;
    if len == 0 || len > buf.len() {
        return None;
    }
    let s = String::from_utf16(&buf[..len]).ok()?;
    let c = s.chars().next()?;
    (!c.is_control() && !c.is_whitespace()).then(|| s.to_lowercase())
}

#[cfg(test)]
mod tests {
    #[test]
    fn digits_have_shifted_chars() {
        // every common layout puts a printable symbol on Shift+2
        let c = super::shifted_char(0x32).expect("shift+2");
        assert!(!c.is_empty() && c != "2", "{c:?}");
    }
}
