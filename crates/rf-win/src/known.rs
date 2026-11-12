use std::path::PathBuf;

use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures,
    FOLDERID_Profile, FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};
use windows::core::GUID;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KnownFolder {
    Desktop,
    Downloads,
    Documents,
    Pictures,
    Music,
    Videos,
    Profile,
}

impl KnownFolder {
    pub const PINNED_DEFAULTS: [KnownFolder; 6] = [
        KnownFolder::Desktop,
        KnownFolder::Downloads,
        KnownFolder::Documents,
        KnownFolder::Pictures,
        KnownFolder::Music,
        KnownFolder::Videos,
    ];

    pub fn title(self) -> &'static str {
        match self {
            KnownFolder::Desktop => "Masaüstü",
            KnownFolder::Downloads => "İndirilenler",
            KnownFolder::Documents => "Belgeler",
            KnownFolder::Pictures => "Resimler",
            KnownFolder::Music => "Müzik",
            KnownFolder::Videos => "Videolar",
            KnownFolder::Profile => "Kullanıcı",
        }
    }

    fn id(self) -> GUID {
        match self {
            KnownFolder::Desktop => FOLDERID_Desktop,
            KnownFolder::Downloads => FOLDERID_Downloads,
            KnownFolder::Documents => FOLDERID_Documents,
            KnownFolder::Pictures => FOLDERID_Pictures,
            KnownFolder::Music => FOLDERID_Music,
            KnownFolder::Videos => FOLDERID_Videos,
            KnownFolder::Profile => FOLDERID_Profile,
        }
    }

    pub fn path(self) -> Option<PathBuf> {
        unsafe {
            let p = SHGetKnownFolderPath(&self.id(), KF_FLAG_DEFAULT, None).ok()?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const _));
            s.map(PathBuf::from)
        }
    }
}

pub fn known_folders() -> Vec<(KnownFolder, PathBuf)> {
    KnownFolder::PINNED_DEFAULTS
        .iter()
        .filter_map(|k| k.path().map(|p| (*k, p)))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn resolves_desktop() {
        let p = super::KnownFolder::Desktop.path().unwrap();
        assert!(p.exists());
    }
}
