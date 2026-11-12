//! Explorer-compatible file clipboard: CF_HDROP + "Preferred DropEffect".

use std::path::PathBuf;

use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, POINT};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::CF_HDROP;
use windows::Win32::UI::Shell::{DROPFILES, DragQueryFileW, HDROP};
use windows::core::w;

use crate::fileops::Transfer;
use crate::wide::wide;

const DROPEFFECT_COPY: u32 = 1;
const DROPEFFECT_MOVE: u32 = 2;

struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn open(owner: HWND) -> anyhow::Result<Guard> {
    // The clipboard can be briefly locked by another app; retry a few times.
    for _ in 0..10 {
        if unsafe { OpenClipboard(Some(owner)) }.is_ok() {
            return Ok(Guard);
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    anyhow::bail!("pano açılamadı")
}

unsafe fn alloc(bytes: &[u8]) -> anyhow::Result<HGLOBAL> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let p = GlobalLock(h) as *mut u8;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        Ok(h)
    }
}

/// Empties the clipboard (so a paste can't pick up what was there before).
pub fn clear(owner: HWND) -> anyhow::Result<()> {
    let _g = open(owner)?;
    unsafe { EmptyClipboard()? };
    Ok(())
}

/// Changes every time the clipboard's content changes.
pub fn sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

/// Puts files on the clipboard for copy or cut. `owner` must be a real window (any thread).
pub fn set_files(owner: HWND, paths: &[PathBuf], kind: Transfer) -> anyhow::Result<()> {
    let header = DROPFILES {
        pFiles: std::mem::size_of::<DROPFILES>() as u32,
        pt: POINT::default(),
        fNC: false.into(),
        fWide: true.into(),
    };
    let mut bytes = Vec::new();
    bytes.extend_from_slice(unsafe {
        std::slice::from_raw_parts(&header as *const _ as *const u8, std::mem::size_of::<DROPFILES>())
    });
    for p in paths {
        for c in wide(p) {
            bytes.extend_from_slice(&c.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&[0, 0]);
    let effect = match kind {
        Transfer::Copy => DROPEFFECT_COPY,
        Transfer::Move => DROPEFFECT_MOVE,
    };
    let _g = open(owner)?;
    unsafe {
        EmptyClipboard()?;
        let h = alloc(&bytes)?;
        SetClipboardData(CF_HDROP.0 as u32, Some(HANDLE(h.0)))?;
        let fmt = RegisterClipboardFormatW(w!("Preferred DropEffect"));
        let h2 = alloc(&effect.to_le_bytes())?;
        SetClipboardData(fmt, Some(HANDLE(h2.0)))?;
    }
    Ok(())
}

pub fn has_files() -> bool {
    unsafe { IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() }
}

/// Reads files + intended operation (cut → Move) from the clipboard.
pub fn get_files(owner: HWND) -> anyhow::Result<Option<(Vec<PathBuf>, Transfer)>> {
    if !has_files() {
        return Ok(None);
    }
    let _g = open(owner)?;
    unsafe {
        let h = GetClipboardData(CF_HDROP.0 as u32)?;
        let hdrop = HDROP(h.0);
        let n = DragQueryFileW(hdrop, u32::MAX, None);
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(hdrop, i, Some(&mut buf));
            out.push(PathBuf::from(String::from_utf16_lossy(&buf[..len])));
        }
        let fmt = RegisterClipboardFormatW(w!("Preferred DropEffect"));
        let mut kind = Transfer::Copy;
        if let Ok(he) = GetClipboardData(fmt) {
            let hg = HGLOBAL(he.0);
            if GlobalSize(hg) >= 4 {
                let p = GlobalLock(hg) as *const u32;
                if !p.is_null() {
                    if *p & DROPEFFECT_MOVE != 0 {
                        kind = Transfer::Move;
                    }
                    let _ = GlobalUnlock(hg);
                }
            }
        }
        Ok(Some((out, kind)))
    }
}

/// Puts a picture on the clipboard: as a bitmap (CF_DIB, every app) and as PNG (the format
/// Discord, browsers and Office paste with transparency). `rgba` is `w * h * 4` bytes, top row
/// first; `png` is the same picture encoded.
pub fn set_image(owner: HWND, w: u32, h: u32, rgba: &[u8], png: &[u8]) -> anyhow::Result<()> {
    use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFOHEADER};
    use windows::Win32::System::Ole::CF_DIB;
    anyhow::ensure!(rgba.len() == (w * h * 4) as usize, "picture size");
    let header = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w as i32,
        // positive height: rows bottom-up, what every reader of CF_DIB expects
        biHeight: h as i32,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: w * h * 4,
        ..Default::default()
    };
    let mut dib = Vec::with_capacity(std::mem::size_of::<BITMAPINFOHEADER>() + rgba.len());
    dib.extend_from_slice(unsafe { std::slice::from_raw_parts(&header as *const _ as *const u8, std::mem::size_of::<BITMAPINFOHEADER>()) });
    for row in (0..h as usize).rev() {
        for px in rgba[row * w as usize * 4..(row + 1) * w as usize * 4].chunks_exact(4) {
            dib.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    let _g = open(owner)?;
    unsafe {
        EmptyClipboard()?;
        SetClipboardData(CF_DIB.0 as u32, Some(HANDLE(alloc(&dib)?.0)))?;
        let fmt = RegisterClipboardFormatW(w!("PNG"));
        SetClipboardData(fmt, Some(HANDLE(alloc(png)?.0)))?;
    }
    Ok(())
}
