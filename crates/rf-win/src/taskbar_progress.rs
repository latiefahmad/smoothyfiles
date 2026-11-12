//! Progress on a window's taskbar button (ITaskbarList3), like Explorer while it copies.

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Shell::{ITaskbarList3, TBPF_ERROR, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED, TaskbarList};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum State {
    /// No bar.
    None,
    /// Working, how far not known yet (counting what to copy).
    Busy,
    /// A green bar at `0.0..=1.0`.
    Normal(f32),
    /// A yellow bar (paused).
    Paused(f32),
    /// A red bar (an operation failed).
    Error(f32),
}

thread_local! {
    // one per thread (COM apartment): the UI thread in practice
    static LIST: Option<ITaskbarList3> = unsafe { CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER).ok().filter(|l| l.HrInit().is_ok()) };
}

/// Shows `state` on the taskbar button of window `hwnd`. Call from the thread that owns the
/// window (COM must be initialized there).
pub fn set(hwnd: isize, state: State) {
    LIST.with(|l| {
        let Some(l) = l else { return };
        let h = HWND(hwnd as *mut _);
        unsafe {
            let (flag, value) = match state {
                State::None => (TBPF_NOPROGRESS, None),
                State::Busy => (TBPF_INDETERMINATE, None),
                State::Normal(v) => (TBPF_NORMAL, Some(v)),
                State::Paused(v) => (TBPF_PAUSED, Some(v)),
                State::Error(v) => (TBPF_ERROR, Some(v)),
            };
            if let Some(v) = value {
                let _ = l.SetProgressValue(h, (v.clamp(0.0, 1.0) * 1000.0) as u64, 1000);
            }
            let _ = l.SetProgressState(h, flag);
        }
    });
}
