//! Rotating images 90° like Files' "Rotate left / right".
//!
//! * JPEG (and other formats whose property handler accepts it): only the EXIF orientation
//!   tag changes — lossless, instant, honored by Explorer, Photos, browsers and our own
//!   thumbnails (they come from the shell).
//! * PNG, BMP, TIFF, GIF (single frame), ICO…: the pixels are rotated through WIC and written
//!   back in the same container format (lossless for those formats), via a temp file that
//!   replaces the original only once it is complete.
//!
//! Blocking + COM: call off the UI thread.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Foundation::GENERIC_WRITE;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, IWICImagingFactory, WICBitmapEncoderNoCache, WICBitmapTransformRotate90,
    WICBitmapTransformRotate270, WICDecodeMetadataCacheOnLoad,
};
use windows::Win32::Storage::EnhancedStorage::PKEY_Photo_Orientation;
use windows::Win32::Storage::FileSystem::{REPLACEFILE_IGNORE_MERGE_ERRORS, ReplaceFileW};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Shell::PropertiesSystem::{GPS_READWRITE, IPropertyStore, SHGetPropertyStoreFromParsingName};
use windows::core::PCWSTR;

use crate::shell::ensure_com;
use crate::wide::wide;

/// Formats we can rotate.
pub fn can_rotate(p: &Path) -> bool {
    let e = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    matches!(e.as_str(), "jpg" | "jpeg" | "jfif" | "jpe" | "png" | "bmp" | "tif" | "tiff" | "gif" | "ico" | "heic" | "webp")
}

fn uses_orientation_tag(p: &Path) -> bool {
    let e = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    matches!(e.as_str(), "jpg" | "jpeg" | "jfif" | "jpe" | "heic")
}

/// Next EXIF orientation after a 90° turn (mirrored variants included).
pub fn next_orientation(cur: u16, clockwise: bool) -> u16 {
    // 1 → 6 → 3 → 8 → 1 (plain); 2 → 7 → 4 → 5 → 2 (mirrored)
    const CW: [(u16, u16); 8] = [(1, 6), (6, 3), (3, 8), (8, 1), (2, 7), (7, 4), (4, 5), (5, 2)];
    let cur = if (1..=8).contains(&cur) { cur } else { 1 };
    if clockwise {
        CW.iter().find(|(a, _)| *a == cur).map_or(6, |(_, b)| *b)
    } else {
        CW.iter().find(|(_, b)| *b == cur).map_or(8, |(a, _)| *a)
    }
}

/// Rotates one image by 90° (clockwise or counter-clockwise).
pub fn rotate(path: &Path, clockwise: bool) -> anyhow::Result<()> {
    anyhow::ensure!(can_rotate(path), "Bu dosya türü döndürülemez");
    ensure_com();
    if uses_orientation_tag(path) {
        match rotate_by_tag(path, clockwise) {
            Ok(()) => return Ok(()),
            // HEIC without a writable handler etc.: fall back to pixels (still lossless for
            // nothing but the container's own codec; JPEG re-encoding is avoided below)
            Err(e) if is_jpeg(path) => return Err(e),
            Err(_) => {}
        }
    }
    rotate_pixels(path, clockwise)
}

fn is_jpeg(p: &Path) -> bool {
    let e = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    matches!(e.as_str(), "jpg" | "jpeg" | "jfif" | "jpe")
}

fn rotate_by_tag(path: &Path, clockwise: bool) -> anyhow::Result<()> {
    let w = wide(path);
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreFromParsingName(PCWSTR(w.as_ptr()), None, GPS_READWRITE)?;
        let cur = store.GetValue(&PKEY_Photo_Orientation).ok().and_then(|v| u16::try_from(&v).ok()).unwrap_or(1);
        let next = next_orientation(cur, clockwise);
        store.SetValue(&PKEY_Photo_Orientation, &PROPVARIANT::from(next))?;
        store.Commit()?;
    }
    Ok(())
}

fn rotate_pixels(path: &Path, clockwise: bool) -> anyhow::Result<()> {
    let tmp: PathBuf = {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".rf-rotate.tmp");
        path.with_file_name(name)
    };
    let res = (|| -> anyhow::Result<()> {
        unsafe {
            let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let src = wide(path);
            let dec = f.CreateDecoderFromFilename(PCWSTR(src.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnLoad)?;
            anyhow::ensure!(dec.GetFrameCount()? == 1, "Çok kareli (animasyonlu) resimler döndürülemez");
            let container = dec.GetContainerFormat()?;
            let frame = dec.GetFrame(0)?;
            let rot = f.CreateBitmapFlipRotator()?;
            rot.Initialize(&frame, if clockwise { WICBitmapTransformRotate90 } else { WICBitmapTransformRotate270 })?;
            let (mut w, mut h) = (0u32, 0u32);
            rot.GetSize(&mut w, &mut h)?;
            let mut fmt = frame.GetPixelFormat()?;
            let (mut dx, mut dy) = (0f64, 0f64);
            let _ = frame.GetResolution(&mut dx, &mut dy);

            let stream = f.CreateStream()?;
            let tw = wide(&tmp);
            stream.InitializeFromFilename(PCWSTR(tw.as_ptr()), GENERIC_WRITE.0)?;
            let enc = f.CreateEncoder(&container, std::ptr::null())?;
            enc.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut fe = None;
            let mut props = None;
            enc.CreateNewFrame(&mut fe, &mut props)?;
            let fe = fe.ok_or_else(|| anyhow::anyhow!("encoder frame"))?;
            fe.Initialize(props.as_ref())?;
            fe.SetSize(w, h)?;
            if dx > 0. && dy > 0. {
                let _ = fe.SetResolution(dy, dx);
            }
            fe.SetPixelFormat(&mut fmt)?;
            fe.WriteSource(&rot, std::ptr::null())?;
            fe.Commit()?;
            enc.Commit()?;
        }
        Ok(())
    })();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // everything was released above: swap the finished file in (keeps attributes / ACLs)
    let (pw, tw) = (wide(path), wide(&tmp));
    let r = unsafe { ReplaceFileW(PCWSTR(pw.as_ptr()), PCWSTR(tw.as_ptr()), PCWSTR::null(), REPLACEFILE_IGNORE_MERGE_ERRORS, None, None) };
    if let Err(e) = r {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!("Resim değiştirilemedi: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orientation_cycle() {
        let mut o = 1;
        for _ in 0..4 {
            o = next_orientation(o, true);
        }
        assert_eq!(o, 1);
        assert_eq!(next_orientation(1, true), 6);
        assert_eq!(next_orientation(1, false), 8);
        assert_eq!(next_orientation(next_orientation(5, true), false), 5);
    }

    /// A JPEG made with WIC: rotating changes only the orientation tag (pixels untouched).
    #[test]
    fn rotates_jpeg_by_tag() {
        use windows::Win32::Graphics::Imaging::{GUID_ContainerFormatJpeg, GUID_WICPixelFormat24bppBGR, WICBitmapCacheOnLoad};
        ensure_com();
        let dir = std::env::temp_dir().join(format!("rf-rotate-jpg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jpg");
        unsafe {
            let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).unwrap();
            let bmp = f.CreateBitmap(16, 8, &GUID_WICPixelFormat24bppBGR, WICBitmapCacheOnLoad).unwrap();
            let stream = f.CreateStream().unwrap();
            let pw = wide(&p);
            stream.InitializeFromFilename(PCWSTR(pw.as_ptr()), GENERIC_WRITE.0).unwrap();
            let enc = f.CreateEncoder(&GUID_ContainerFormatJpeg, std::ptr::null()).unwrap();
            enc.Initialize(&stream, WICBitmapEncoderNoCache).unwrap();
            let (mut fe, mut props) = (None, None);
            enc.CreateNewFrame(&mut fe, &mut props).unwrap();
            let fe = fe.unwrap();
            fe.Initialize(props.as_ref()).unwrap();
            fe.SetSize(16, 8).unwrap();
            let mut fmt = GUID_WICPixelFormat24bppBGR;
            fe.SetPixelFormat(&mut fmt).unwrap();
            fe.WriteSource(&bmp, std::ptr::null()).unwrap();
            fe.Commit().unwrap();
            enc.Commit().unwrap();
        }
        let orient = || unsafe {
            let w = wide(&p);
            let s: IPropertyStore =
                SHGetPropertyStoreFromParsingName(PCWSTR(w.as_ptr()), None, windows::Win32::UI::Shell::PropertiesSystem::GPS_DEFAULT).unwrap();
            s.GetValue(&PKEY_Photo_Orientation).ok().and_then(|v| u16::try_from(&v).ok()).unwrap_or(1)
        };
        rotate(&p, true).unwrap();
        assert_eq!(orient(), 6);
        rotate(&p, false).unwrap();
        rotate(&p, false).unwrap();
        assert_eq!(orient(), 8);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A 3×2 BMP rotated clockwise becomes 2×3 with the pixels moved, written in place.
    #[test]
    fn rotates_bmp_pixels() {
        let dir = std::env::temp_dir().join(format!("rf-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.bmp");
        // 24-bit BMP, 3×2, bottom-up rows padded to 4 bytes; top-left pixel red, rest white
        let (w, h) = (3u32, 2u32);
        let row = ((w * 3 + 3) / 4) * 4;
        let mut px = vec![255u8; (row * h) as usize];
        // top row is stored last (bottom-up); first pixel BGR = red
        let top = (row * (h - 1)) as usize;
        px[top..top + 3].copy_from_slice(&[0, 0, 255]);
        let mut f = Vec::new();
        f.extend_from_slice(b"BM");
        f.extend_from_slice(&(54 + px.len() as u32).to_le_bytes());
        f.extend_from_slice(&[0; 4]);
        f.extend_from_slice(&54u32.to_le_bytes());
        f.extend_from_slice(&40u32.to_le_bytes());
        f.extend_from_slice(&(w as i32).to_le_bytes());
        f.extend_from_slice(&(h as i32).to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&24u16.to_le_bytes());
        f.extend_from_slice(&[0; 24]);
        f.extend_from_slice(&px);
        std::fs::write(&p, &f).unwrap();

        rotate(&p, true).unwrap();
        let out = std::fs::read(&p).unwrap();
        let ow = i32::from_le_bytes(out[18..22].try_into().unwrap());
        let oh = i32::from_le_bytes(out[22..26].try_into().unwrap()).abs();
        assert_eq!((ow, oh), (2, 3));
        assert!(!dir.join("t.bmp.rf-rotate.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
