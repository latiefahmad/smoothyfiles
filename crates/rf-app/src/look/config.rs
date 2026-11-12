//! DirectLook ayarları: kendi dosyası `directlook.toml` (smoothyfiles'ın `settings.toml`'u ile iki
//! süreç birbirinin değişikliğini ezmesin diye ayrı). Her değişiklik diskten taze okunur, üzerine
//! yazılır, kaydedilir.

use std::path::{Path, PathBuf};

use gpui_kit::{App, Global};
use serde::{Deserialize, Serialize};

use crate::settings::{LookBackdrop, LookImageTheme};

/// DirectLook ayarları (varsayılanlar QuickLook'unkiler).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LookSettings {
    /// Explorer ve masaüstünde de Boşluk ile önizleme (sistem geneli klavye kancası).
    pub look_system_wide: bool,
    /// QuickLook.exe çalışırken de sistem geneli kısayolu aç.
    pub look_ignore_quicklook: bool,
    pub look_tray_icon: bool,
    pub look_backdrop: LookBackdrop,
    /// Arka plan rengi (0xRRGGBB), `None` = arka plan olduğu gibi.
    pub look_backdrop_color: Option<u32>,
    pub look_topmost: bool,
    pub look_show_in_taskbar: bool,
    /// Boşluğu basılı tutup bırakınca kapan.
    pub look_hold_to_close: bool,
    pub look_close_on_lost_focus: bool,
    pub look_show_reload: bool,
    pub look_auto_reload: bool,
    pub look_focus_on_open: bool,
    pub look_transition: bool,
    pub look_software_render: bool,
    /// true: yalnız `look_ext_allowlist`'teki uzantılar; false: `look_ext_blocklist` dışındakiler.
    pub look_ext_allow_mode: bool,
    pub look_ext_allowlist: String,
    pub look_ext_blocklist: String,
    pub look_image_theme: LookImageTheme,
    pub look_svg_web: bool,
    pub look_loop: bool,
    pub look_volume: f32,
    pub look_text_line_numbers: bool,
    pub look_text_wrap: bool,
    pub look_text_detect_format: bool,
    pub look_text_font: String,
    pub look_text_font_size: f32,
}

impl Default for LookSettings {
    fn default() -> Self {
        Self {
            look_system_wide: false,
            look_ignore_quicklook: false,
            look_tray_icon: true,
            look_backdrop: LookBackdrop::Mica,
            look_backdrop_color: None,
            look_topmost: true,
            look_show_in_taskbar: false,
            look_hold_to_close: true,
            look_close_on_lost_focus: false,
            look_show_reload: false,
            look_auto_reload: false,
            look_focus_on_open: false,
            look_transition: true,
            look_software_render: false,
            look_ext_allow_mode: false,
            look_ext_allowlist: String::new(),
            look_ext_blocklist: ".insv".into(),
            look_image_theme: LookImageTheme::Dark,
            look_svg_web: true,
            look_loop: false,
            look_volume: 1.0,
            look_text_line_numbers: true,
            look_text_wrap: true,
            look_text_detect_format: true,
            look_text_font: String::new(),
            look_text_font_size: 14.0,
        }
    }
}

impl Global for LookSettings {}

pub fn path() -> Option<PathBuf> {
    crate::settings::app_dir().map(|d| d.join("directlook.toml"))
}

impl LookSettings {
    pub fn load() -> LookSettings {
        path().map(|p| LookSettings::load_from(&p)).unwrap_or_default()
    }

    pub fn load_from(p: &Path) -> LookSettings {
        match rf_core::read_config(p) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                log::warn!("directlook.toml: {e}");
                LookSettings::default()
            }),
            Err(_) => LookSettings::default(),
        }
    }

    pub fn save(&self) {
        if let Some(p) = path() {
            self.save_to(&p);
        }
    }

    /// Yaz-sonra-yeniden-adlandır (yarım dosya kalmaz).
    pub fn save_to(&self, p: &Path) {
        static LOCK: parking_lot::Mutex<u64> = parking_lot::Mutex::new(0);
        let mut n = LOCK.lock();
        *n += 1;
        let Ok(text) = toml::to_string_pretty(self) else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = p.with_extension(format!("toml.{}.{}.tmp", std::process::id(), *n));
        if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, p).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Ayarı değiştirir: diskteki güncel hâli okunur (öteki süreç de yazmış olabilir), değişiklik
/// uygulanır, kaydedilir; bu süreçteki kopya güncellenir.
pub fn update(cx: &mut App, f: impl FnOnce(&mut LookSettings)) {
    let mut s = LookSettings::load();
    f(&mut s);
    s.save();
    cx.set_global(s);
    cx.refresh_windows();
}

/// Bu süreçteki kopya (yoksa diskten okunur).
pub fn get(cx: &mut App) -> &LookSettings {
    if cx.try_global::<LookSettings>().is_none() {
        cx.set_global(LookSettings::load());
    }
    cx.global::<LookSettings>()
}

#[cfg(test)]
mod tests {
    use super::LookSettings;

    #[test]
    fn defaults_follow_quicklook() {
        let s = LookSettings::default();
        assert!(s.look_topmost && !s.look_system_wide && s.look_hold_to_close && s.look_transition && !s.look_focus_on_open);
        assert_eq!(s.look_ext_blocklist, ".insv");
        assert_eq!(s.look_volume, 1.0);
    }

    #[test]
    fn round_trip_and_unknown_or_missing_keys() {
        let dir = std::env::temp_dir().join(format!("rf-lookcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("directlook.toml");
        let mut s = LookSettings::default();
        s.look_topmost = false;
        s.look_volume = 0.5;
        s.save_to(&p);
        assert_eq!(LookSettings::load_from(&p), s);
        // eski/eksik/bilinmeyen alanlar: varsayılanla doldurulur, dosya bozuk sayılmaz
        std::fs::write(&p, "look_topmost = false\nbilinmeyen = 3\n").unwrap();
        let l = LookSettings::load_from(&p);
        assert!(!l.look_topmost && l.look_transition);
        // dosya yok: varsayılan
        assert_eq!(LookSettings::load_from(&dir.join("yok.toml")), LookSettings::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
