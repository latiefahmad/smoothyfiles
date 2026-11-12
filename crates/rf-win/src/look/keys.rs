//! Tuş süzgeci: klavye kancasından gelen tuş olaylarını Hızlı Bakış eylemlerine çevirir.
//! Saf durum makinesi; Windows'a bağımlı değil.

pub const VK_RETURN: u32 = 0x0D;
pub const VK_ESCAPE: u32 = 0x1B;
pub const VK_SPACE: u32 = 0x20;
pub const VK_LEFT: u32 = 0x25;
pub const VK_UP: u32 = 0x26;
pub const VK_RIGHT: u32 = 0x27;
pub const VK_DOWN: u32 = 0x28;
pub const VK_F5: u32 = 0x74;
pub const VK_F11: u32 = 0x7A;

/// Boşluk bu kadar ya da daha uzun basılı tutulup bırakılırsa pencere kapanır.
pub const HOLD_CLOSE_MS: u64 = 750;
/// Başka bir tuşa basıldıktan sonra bu süre boyunca hiçbir tuş eylem üretmez (yazı yazarken açılmasın).
pub const GUARD_MS: u64 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookKey { Space, Enter, Escape, Up, Down, Left, Right, F5, F11 }

impl LookKey {
    pub fn from_vk(vk: u32) -> Option<LookKey> {
        Some(match vk {
            VK_SPACE => LookKey::Space,
            VK_RETURN => LookKey::Enter,
            VK_ESCAPE => LookKey::Escape,
            VK_UP => LookKey::Up,
            VK_DOWN => LookKey::Down,
            VK_LEFT => LookKey::Left,
            VK_RIGHT => LookKey::Right,
            VK_F5 => LookKey::F5,
            VK_F11 => LookKey::F11,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookAction { Toggle, RunAndClose, Close, Reload, Fullscreen, SelectionMoved }

#[derive(Debug, Default)]
pub struct KeyGate {
    /// Son geçersiz tuş basma anı.
    last_invalid: Option<u64>,
    /// Boşluk basılıysa basıldığı an.
    space_down: Option<u64>,
}

impl KeyGate {
    pub fn new() -> KeyGate { KeyGate::default() }

    /// Ön plandaki pencere değişti: yazma koruması biter.
    pub fn foreground_changed(&mut self) { self.last_invalid = None; }

    pub fn on_key(&mut self, vk: u32, down: bool, modifiers: bool, now_ms: u64, target_ok: bool) -> Option<LookAction> {
        let Some(key) = LookKey::from_vk(vk) else {
            if down { self.last_invalid = Some(now_ms); }
            return None;
        };
        // Boşluk bırakılınca durum her koşulda temizlenir (koruma süresi içinde bile), yoksa sonraki
        // basış otomatik tekrar sanılır ve bırakışı bayat bir süreyle eylem üretir.
        let released_at = if key == LookKey::Space && !down { self.space_down.take() } else { None };
        if let Some(t) = self.last_invalid {
            if now_ms.saturating_sub(t) < GUARD_MS { return None; }
            self.last_invalid = None;
        }
        if modifiers || !target_ok {
            return None;
        }
        match (key, down) {
            (LookKey::Space, true) => {
                if self.space_down.is_some() { return None; } // otomatik tekrar
                self.space_down = Some(now_ms);
                Some(LookAction::Toggle)
            }
            (LookKey::Space, false) => {
                // basılı tutup bırakma yalnız kapatır (açık pencereyi kapatmak için basılıp tutulduysa
                // bırakış onu yeniden açmamalı)
                let held = released_at?;
                (now_ms.saturating_sub(held) >= HOLD_CLOSE_MS).then_some(LookAction::Close)
            }
            (LookKey::Enter, true) => Some(LookAction::RunAndClose),
            (LookKey::Escape, false) => Some(LookAction::Close),
            (LookKey::F5, true) => Some(LookAction::Reload),
            (LookKey::F11, true) => Some(LookAction::Fullscreen),
            (LookKey::Up | LookKey::Down | LookKey::Left | LookKey::Right, false) => Some(LookAction::SelectionMoved),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] fn space_toggles() { let mut g = KeyGate::new(); assert_eq!(g.on_key(VK_SPACE, true, false, 0, true), Some(LookAction::Toggle)); assert_eq!(g.on_key(VK_SPACE, false, false, 100, true), None); }
    #[test] fn holding_space_closes_on_release() { let mut g = KeyGate::new(); g.on_key(VK_SPACE, true, false, 0, true); assert_eq!(g.on_key(VK_SPACE, true, false, 300, true), None); assert_eq!(g.on_key(VK_SPACE, false, false, 800, true), Some(LookAction::Close)); }
    /// Boşluk basılıyken başka tuşa basılıp Boşluk koruma süresinde bırakılırsa durum takılı kalmamalı.
    #[test] fn space_release_inside_guard_is_not_lost() {
        let mut g = KeyGate::new();
        assert_eq!(g.on_key(VK_SPACE, true, false, 0, true), Some(LookAction::Toggle));
        g.on_key(0x41, true, false, 100, true);
        assert_eq!(g.on_key(VK_SPACE, false, false, 900, true), None);
        // sonraki basış otomatik tekrar sanılmamalı, bırakışı sahte bir eylem üretmemeli
        assert_eq!(g.on_key(VK_SPACE, true, false, 3000, true), Some(LookAction::Toggle));
        assert_eq!(g.on_key(VK_SPACE, false, false, 3100, true), None);
    }
    #[test] fn typing_guard() { let mut g = KeyGate::new(); assert_eq!(g.on_key(0x41 /*A*/, true, false, 0, true), None); assert_eq!(g.on_key(VK_SPACE, true, false, 500, true), None); assert_eq!(g.on_key(VK_SPACE, true, false, 1600, true), Some(LookAction::Toggle)); }
    #[test] fn foreground_change_clears_guard() { let mut g = KeyGate::new(); g.on_key(0x41, true, false, 0, true); g.foreground_changed(); assert_eq!(g.on_key(VK_SPACE, true, false, 10, true), Some(LookAction::Toggle)); }
    #[test] fn modifiers_ignored() { let mut g = KeyGate::new(); assert_eq!(g.on_key(VK_SPACE, true, true, 0, true), None); }
    #[test] fn other_keys() { let mut g = KeyGate::new(); assert_eq!(g.on_key(VK_RETURN, true, false, 0, true), Some(LookAction::RunAndClose)); assert_eq!(g.on_key(VK_ESCAPE, false, false, 10, true), Some(LookAction::Close)); assert_eq!(g.on_key(VK_DOWN, false, false, 20, true), Some(LookAction::SelectionMoved)); assert_eq!(g.on_key(VK_F11, true, false, 30, true), Some(LookAction::Fullscreen)); }
    #[test] fn wrong_target() { let mut g = KeyGate::new(); assert_eq!(g.on_key(VK_SPACE, true, false, 0, false), None); }
}
