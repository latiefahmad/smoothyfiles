//! Read-only Windows system parameters the UI follows.

use windows::Win32::UI::WindowsAndMessaging::{SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW};

/// Windows' "Animation effects" (Settings → Accessibility → Visual effects). True when it can't
/// be read (the Windows default).
pub fn client_area_animation() -> bool {
    // a Win32 BOOL
    let mut on: i32 = 1;
    unsafe {
        if SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, Some(&mut on as *mut i32 as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).is_err() {
            return true;
        }
    }
    on != 0
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_animation_effects() {
        // whatever the machine says, the call must not fail or panic
        let _ = super::client_area_animation();
    }
}
