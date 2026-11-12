//! Type-specific file actions from Files' context menu: set as desktop background, install a
//! font (for the current user — no administrator rights, no UAC). Blocking: off the UI thread.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::AddFontResourceW;
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_BROADCAST, SMTO_ABORTIFHUNG, SPI_SETDESKWALLPAPER, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE, SendMessageTimeoutW,
    SystemParametersInfoW, WM_FONTCHANGE,
};
use windows::core::PCWSTR;

use crate::wide::wide;

pub fn is_image(p: &Path) -> bool {
    let e = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    matches!(e.as_str(), "jpg" | "jpeg" | "png" | "bmp" | "gif" | "tif" | "tiff" | "webp" | "jfif" | "heic" | "avif")
}

pub fn is_font(p: &Path) -> bool {
    let e = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    matches!(e.as_str(), "ttf" | "otf" | "ttc" | "fon")
}

/// Executables that "Run as administrator" applies to.
pub fn is_runnable(p: &Path) -> bool {
    let e = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    matches!(e.as_str(), "exe" | "bat" | "cmd" | "msi" | "ps1" | "com")
}

pub fn set_wallpaper(path: &Path) -> anyhow::Result<()> {
    let mut w = wide(path);
    unsafe {
        SystemParametersInfoW(SPI_SETDESKWALLPAPER, 0, Some(w.as_mut_ptr() as *mut _), SPIF_UPDATEINIFILE | SPIF_SENDCHANGE)?;
    }
    Ok(())
}

/// Installs a font for the current user: copies it into
/// `%LOCALAPPDATA%\Microsoft\Windows\Fonts`, registers it under HKCU and tells running apps.
/// Returns the installed copy.
pub fn install_font_for_user(path: &Path) -> anyhow::Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").ok_or_else(|| anyhow::anyhow!("LOCALAPPDATA yok"))?;
    let dir = PathBuf::from(base).join("Microsoft").join("Windows").join("Fonts");
    std::fs::create_dir_all(&dir)?;
    let name = path.file_name().ok_or_else(|| anyhow::anyhow!("geçersiz ad"))?;
    let dest = dir.join(name);
    if !dest.exists() {
        std::fs::copy(path, &dest)?;
    }
    // registry value name: the family as Windows would show it is not known without parsing the
    // font; the file stem plus the type works (Windows reads the real names from the file)
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let kind = match path.extension().map(|e| e.to_string_lossy().to_lowercase()).as_deref() {
        Some("otf") => "(OpenType)",
        _ => "(TrueType)",
    };
    let key = windows_registry::CURRENT_USER.create(r"Software\Microsoft\Windows NT\CurrentVersion\Fonts")?;
    key.set_string(&format!("{stem} {kind}"), dest.to_string_lossy().as_ref())?;
    let w = wide(&dest);
    unsafe {
        AddFontResourceW(PCWSTR(w.as_ptr()));
        let _ = SendMessageTimeoutW(HWND_BROADCAST, WM_FONTCHANGE, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG, 1000, None);
    }
    Ok(dest)
}
