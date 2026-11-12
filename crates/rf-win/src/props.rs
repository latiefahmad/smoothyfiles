//! Media/document properties from the Windows property system (duration, resolution, frame
//! rate, bitrate, camera…), formatted by Windows itself (localized). Blocking: off the UI thread.

use std::path::Path;

use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Storage::EnhancedStorage::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::PropertiesSystem::{
    GPS_BESTEFFORT, GPS_OPENSLOWITEM, IPropertyStore, PDFF_DEFAULT, PSFormatForDisplayAlloc,
    SHGetPropertyStoreFromParsingName,
};
use windows::core::PCWSTR;

use crate::shell::ensure_com;
use crate::wide::wide;

/// (label, key) pairs shown in the preview pane, in display order.
fn wanted() -> Vec<(&'static str, PROPERTYKEY)> {
    vec![
        ("Süre", PKEY_Media_Duration),
        ("Çözünürlük", PKEY_Video_FrameWidth), // merged with height below
        ("Kare hızı", PKEY_Video_FrameRate),
        ("Video bit hızı", PKEY_Video_EncodingBitrate),
        ("Toplam bit hızı", PKEY_Video_TotalBitrate),
        ("Ses bit hızı", PKEY_Audio_EncodingBitrate),
        ("Örnekleme", PKEY_Audio_SampleRate),
        ("Kanal", PKEY_Audio_ChannelCount),
        ("Boyutlar", PKEY_Image_Dimensions),
        ("Bit derinliği", PKEY_Image_BitDepth),
        ("Kamera", PKEY_Photo_CameraModel),
        ("Çekim tarihi", PKEY_Photo_DateTaken),
        ("Sanatçı", PKEY_Music_Artist),
        ("Albüm", PKEY_Music_AlbumTitle),
        ("Başlık", PKEY_Title),
        ("Sayfa", PKEY_Document_PageCount),
        ("Yazar", PKEY_Author),
    ]
}

fn display(store: &IPropertyStore, key: &PROPERTYKEY) -> Option<String> {
    unsafe {
        let pv = store.GetValue(key).ok()?;
        let s = PSFormatForDisplayAlloc(key, &pv, PDFF_DEFAULT).ok()?;
        let out = s.to_string().ok();
        CoTaskMemFree(Some(s.0 as *const _));
        out.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
    }
}

/// Returns non-empty properties as (label, value).
pub fn media_props(path: &Path) -> Vec<(&'static str, String)> {
    ensure_com();
    let w = wide(path);
    // BESTEFFORT: don't force slow handlers to fully parse; never hydrates cloud files by itself
    let store: IPropertyStore =
        match unsafe { SHGetPropertyStoreFromParsingName(PCWSTR(w.as_ptr()), None, GPS_BESTEFFORT | GPS_OPENSLOWITEM) } {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
    let mut out = Vec::new();
    for (label, key) in wanted() {
        if key == PKEY_Video_FrameWidth {
            if let (Some(wd), Some(h)) = (display(&store, &PKEY_Video_FrameWidth), display(&store, &PKEY_Video_FrameHeight)) {
                out.push((label, format!("{wd} × {h}")));
            }
            continue;
        }
        if let Some(v) = display(&store, &key) {
            out.push((label, v));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn image_dimensions() {
        let jpg = std::fs::read_dir(r"C:\Windows\Web\Wallpaper\Windows")
            .ok()
            .and_then(|mut d| d.find_map(|e| e.ok().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jpg"))));
        if let Some(p) = jpg {
            let props = super::media_props(&p);
            assert!(props.iter().any(|(l, _)| *l == "Boyutlar"), "{props:?}");
        }
    }
}
