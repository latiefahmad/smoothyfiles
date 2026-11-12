//! Shell icons and thumbnails via IShellItemImageFactory, decoded to BGRA pixels.
//! Blocking + COM: call from the worker pool (`ThumbPool`), never the UI thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DIBSECTION, DeleteObject, GetDC, GetDIBits, GetObjectW, HBITMAP,
    HGDIOBJ, ReleaseDC,
};
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL, THREAD_MODE_BACKGROUND_BEGIN};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
    SIIGBF_RESIZETOFIT, SIIGBF_THUMBNAILONLY,
};
use windows::core::PCWSTR;

use crate::shell::ensure_com;
use crate::wide::wide;

/// Straight-alpha BGRA, top-down rows.
#[derive(Clone)]
pub struct Bgra {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Icon,
    /// Real thumbnail if the shell has one; otherwise falls back to the icon.
    Thumbnail,
}

/// Starts GDI+ once and never shuts it down.
///
/// The shell starts and stops GDI+ around icon extraction. When the last user stops it,
/// `GdiplusShutdown` holds the GDI+ lock while waiting for its helper thread to exit, and
/// a thread exit needs the loader lock. Some third-party thumbnail handlers (Corel's
/// FileInfoProvider.dll) call `GdiplusStartup` from DllMain, i.e. under the loader lock,
/// and then wait for the GDI+ lock: a deadlock that also blocks every new thread of the
/// process (the folder listing never starts). With our own reference held for the whole
/// process lifetime, the shell's shutdown calls only decrement the count and never wait.
pub fn keep_gdiplus_alive() {
    use windows::Win32::Graphics::GdiPlus::{GdiplusStartup, GdiplusStartupInput, Status};
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let mut token = 0usize;
        let input = GdiplusStartupInput { GdiplusVersion: 1, ..Default::default() };
        let st = GdiplusStartup(&mut token, &input, std::ptr::null_mut());
        if st != Status(0) {
            log::warn!("GdiplusStartup: {st:?}");
        }
    });
}

/// A design file's thumbnail from the file itself (`crate::design`), not from the shell: the
/// program's own thumbnail provider may be missing, or draw only the page (CorelDRAW's is blank
/// when the design sits beside the page). An Illustrator file that is a PDF and has no saved
/// thumbnail gets its first artboard drawn.
fn design_thumbnail(path: &Path, size: u32) -> Option<Bgra> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if !crate::design::is_design_ext(&ext) {
        return None;
    }
    if let Some(img) = crate::design::preview(path) {
        return Some(crate::look::decode::bgra_within(img, size));
    }
    if ext != "ai" {
        return None;
    }
    let mut head = [0u8; 5];
    use std::io::Read;
    std::fs::File::open(path).ok()?.read_exact(&mut head).ok()?;
    if &head != b"%PDF-" {
        return None;
    }
    // Windows' PDF engine on a thread of its own (it wants a multithreaded apartment)
    let p = path.to_path_buf();
    std::thread::spawn(move || -> Option<Bgra> {
        let pdf = crate::look::pdf::Pdf::open(&p, None).ok()?;
        let (w, h) = pdf.page_size(0).ok()?;
        let width = if h > w { (size as f32 * w / h.max(1.0)).round() as u32 } else { size };
        pdf.render(0, width.max(16)).ok()
    })
    .join()
    .ok()
    .flatten()
}

pub fn get_image(path: &Path, size: u32, kind: Kind) -> anyhow::Result<Bgra> {
    if matches!(kind, Kind::Thumbnail) {
        if let Some(b) = design_thumbnail(path, size) {
            return Ok(b);
        }
    }
    keep_gdiplus_alive();
    ensure_com();
    let w = wide(path);
    let factory: IShellItemImageFactory = unsafe { SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None)? };
    let sz = SIZE { cx: size as i32, cy: size as i32 };
    let hbmp = match kind {
        Kind::Icon => unsafe { factory.GetImage(sz, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK)? },
        Kind::Thumbnail => unsafe {
            factory
                .GetImage(sz, SIIGBF(SIIGBF_THUMBNAILONLY.0 | SIIGBF_RESIZETOFIT.0 | SIIGBF_BIGGERSIZEOK.0))
                .or_else(|_| factory.GetImage(sz, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK))?
        },
    };
    let r = hbitmap_to_bgra(hbmp);
    unsafe {
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
    }
    r
}

fn hbitmap_to_bgra(hbmp: HBITMAP) -> anyhow::Result<Bgra> {
    unsafe {
        let mut ds = DIBSECTION::default();
        let n = GetObjectW(HGDIOBJ(hbmp.0), std::mem::size_of::<DIBSECTION>() as i32, Some(&mut ds as *mut _ as *mut _));
        anyhow::ensure!(n as usize >= std::mem::size_of::<BITMAP>(), "GetObject failed");
        let bm = ds.dsBm;
        anyhow::ensure!(!bm.bmBits.is_null() && bm.bmBitsPixel == 32, "unsupported bitmap");
        let w = bm.bmWidth as u32;
        let h = bm.bmHeight.unsigned_abs();
        // Let GDI hand the rows over top-down: the DIB's own row order can't be trusted (for
        // EXIF-rotated photos the shell returns a top-down section that GetObject reports as
        // bottom-up, which turned those thumbnails upside down).
        let mut data = vec![0u8; (w * h * 4) as usize];
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let dc = GetDC(None);
        let lines = GetDIBits(dc, hbmp, 0, h, Some(data.as_mut_ptr() as *mut _), &mut bmi, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        anyhow::ensure!(lines as u32 == h, "GetDIBits failed");
        let any_alpha = data.chunks_exact(4).any(|p| p[3] != 0);
        if !any_alpha {
            // Opaque thumbnails often come with alpha = 0.
            for p in data.chunks_exact_mut(4) {
                p[3] = 255;
            }
        } else {
            // Shell bitmaps are premultiplied; GPUI wants straight alpha.
            for p in data.chunks_exact_mut(4) {
                let a = p[3] as u32;
                if a != 0 && a != 255 {
                    p[0] = ((p[0] as u32 * 255 + a / 2) / a).min(255) as u8;
                    p[1] = ((p[1] as u32 * 255 + a / 2) / a).min(255) as u8;
                    p[2] = ((p[2] as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        Ok(Bgra { width: w, height: h, data })
    }
}

/// Extensions whose icon differs per file (must not be cached by extension).
pub fn has_unique_icon(ext: &str) -> bool {
    matches!(ext, "exe" | "lnk" | "ico" | "url" | "cur" | "ani" | "scr" | "cpl" | "msc" | "appref-ms" | "website")
}

pub struct Request {
    pub key: u64,
    pub path: PathBuf,
    pub size: u32,
    pub kind: Kind,
}

pub struct Response {
    pub key: u64,
    pub image: Option<Arc<Bgra>>,
    /// Request was discarded (backlog) — not a failure; may be retried.
    pub dropped: bool,
}

/// Background worker pool for icon/thumbnail extraction. Newest requests are served first
/// (they belong to what the user is looking at right now).
pub struct ThumbPool {
    stack: Arc<Mutex<Vec<Request>>>,
    wake: Sender<()>,
    on_done: Arc<dyn Fn(Response) + Send + Sync>,
}

impl ThumbPool {
    pub fn new(workers: usize, on_done: impl Fn(Response) + Send + Sync + 'static) -> Self {
        let stack: Arc<Mutex<Vec<Request>>> = Arc::new(Mutex::new(Vec::new()));
        let (wake, rx): (Sender<()>, Receiver<()>) = crossbeam_channel::unbounded();
        let on_done: Arc<dyn Fn(Response) + Send + Sync> = Arc::new(on_done);
        keep_gdiplus_alive();
        for i in 0..workers {
            let stack = stack.clone();
            let rx = rx.clone();
            let on_done = on_done.clone();
            std::thread::Builder::new()
                .name(format!("rf-thumb-{i}"))
                .spawn(move || {
                    unsafe {
                        // Background mode lowers CPU *and* I/O priority: never compete with the user's work.
                        let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
                        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                    }
                    ensure_com();
                    while rx.recv().is_ok() {
                        loop {
                            let req = stack.lock().pop();
                            let Some(req) = req else { break };
                            let image = get_image(&req.path, req.size, req.kind).ok().map(Arc::new);
                            on_done(Response { key: req.key, image, dropped: false });
                        }
                    }
                })
                .ok();
        }
        Self { stack, wake, on_done }
    }

    pub fn request(&self, req: Request) {
        let mut s = self.stack.lock();
        // Bound the backlog: drop the oldest (scrolled-away) requests.
        let dropped: Vec<Request> = if s.len() > 512 { s.drain(0..256).collect() } else { Vec::new() };
        s.push(req);
        drop(s);
        // Tell the cache so those slots don't stay "pending" forever (they will be re-requested).
        for d in dropped {
            (self.on_done)(Response { key: d.key, image: None, dropped: true });
        }
        let _ = self.wake.send(());
    }

    /// Drops queued work (e.g. on navigation).
    pub fn clear(&self) {
        self.stack.lock().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JPEG with a red top-left corner, turned clockwise via its EXIF tag: the shell
    /// thumbnail must show the red corner at the top right (it came out upside down once).
    #[test]
    fn exif_rotated_thumbnail_is_upright() {
        use windows::Win32::Foundation::GENERIC_WRITE;
        use windows::Win32::Graphics::Imaging::*;
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
        ensure_com();
        let dir = std::env::temp_dir().join(format!("rf-thumb-exif-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("r.jpg");
        let (w, h) = (64u32, 40u32);
        let mut px = vec![0u8; (w * h * 3) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 3) as usize;
                let red = x < 16 && y < 16;
                px[i..i + 3].copy_from_slice(if red { &[0, 0, 255] } else { &[200, 120, 40] });
            }
        }
        unsafe {
            let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).unwrap();
            let bmp = f.CreateBitmapFromMemory(w, h, &GUID_WICPixelFormat24bppBGR, w * 3, &px).unwrap();
            let stream = f.CreateStream().unwrap();
            let pw = wide(&p);
            stream.InitializeFromFilename(PCWSTR(pw.as_ptr()), GENERIC_WRITE.0).unwrap();
            let enc = f.CreateEncoder(&GUID_ContainerFormatJpeg, std::ptr::null()).unwrap();
            enc.Initialize(&stream, WICBitmapEncoderNoCache).unwrap();
            let (mut fe, mut props) = (None, None);
            enc.CreateNewFrame(&mut fe, &mut props).unwrap();
            let fe = fe.unwrap();
            fe.Initialize(props.as_ref()).unwrap();
            fe.SetSize(w, h).unwrap();
            let mut fmt = GUID_WICPixelFormat24bppBGR;
            fe.SetPixelFormat(&mut fmt).unwrap();
            fe.WriteSource(&bmp, std::ptr::null()).unwrap();
            fe.Commit().unwrap();
            enc.Commit().unwrap();
        }
        crate::rotate::rotate(&p, true).unwrap();
        let img = get_image(&p, 64, Kind::Thumbnail).unwrap();
        assert!(img.height > img.width, "portrait after the turn: {}x{}", img.width, img.height);
        let at = |x: u32, y: u32| {
            let i = ((y * img.width + x) * 4) as usize;
            (img.data[i + 2], img.data[i + 1], img.data[i])
        };
        let (r, g, b) = at(img.width - 3, 3);
        assert!(r > 180 && g < 90 && b < 90, "top-right should be red, got {:?}", (r, g, b));
        let (r, _, _) = at(img.width - 3, img.height - 3);
        assert!(r < 120, "bottom-right should not be red");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn icon_for_exe_and_folder() {
        let exe = PathBuf::from(r"C:\Windows\explorer.exe");
        let img = get_image(&exe, 32, Kind::Icon).unwrap();
        assert!(img.width >= 32 && img.height >= 32);
        assert!(img.data.chunks_exact(4).any(|p| p[3] > 0));
        let dir = get_image(Path::new(r"C:\Windows"), 32, Kind::Icon).unwrap();
        assert!(dir.width > 0);
    }
}
