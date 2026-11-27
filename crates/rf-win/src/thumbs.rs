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
    /// The file's own picture or nothing (an error): for types that may or may not have one (see `has_provider`), where the
    /// caller has something better to show than this one file's icon.
    ThumbnailOnly,
}

/// Whether the shell has a thumbnail provider for files of this extension (lower case, no dot): one a program registered for the
/// type itself, for its class or for its kind. Read from the registry once for each extension and remembered until the program ends
/// (a provider installed meanwhile is seen after a restart).
///
/// WHY: thumbnails were asked for a fixed list of types only (pictures, videos, PDFs and a few more), so a type that a thumbnail
/// extension such as SageThumbs 2K draws (`.pcraft`, `.swf`, a sound file's cover) stayed an icon here while Explorer showed its
/// picture.
pub fn has_provider(ext: &str) -> bool {
    static KNOWN: Mutex<Option<std::collections::HashMap<String, bool>>> = Mutex::new(None);
    if ext.is_empty() || ext.len() > 32 || !ext.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '+' | '~')) {
        return false;
    }
    if let Some(&known) = KNOWN.lock().get_or_insert_with(Default::default).get(ext) {
        return known;
    }
    // (asked outside the lock: two threads may both ask about a new extension, and both get the same answer)
    let found = provider_of(ext).is_some_and(|p| Some(&p) != none_provider().as_ref());
    KNOWN.lock().get_or_insert_with(Default::default).insert(ext.to_string(), found);
    found
}

/// What the shell answers for an extension nobody registered: a provider that stands for every file there is (should one exist)
/// is no sign that this type has a picture of its own.
fn none_provider() -> Option<String> {
    static NONE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    NONE.get_or_init(|| provider_of("rustyfiles-no-such-type")).clone()
}

/// The class of the thumbnail provider the shell would use for this extension (IThumbnailProvider, or the older IExtractImage).
fn provider_of(ext: &str) -> Option<String> {
    use windows::Win32::UI::Shell::{ASSOCF_NONE, ASSOCSTR_SHELLEXTENSION, AssocQueryStringW};
    let dotted = wide(format!(".{ext}"));
    ["{E357FCCD-A995-4576-B01F-234630154E96}", "{BB2E617C-0920-11D1-9A0B-00C04FC2D6C1}"].iter().find_map(|interface| {
        let interface = wide(interface);
        let mut buf = [0u16; 64];
        let mut len = buf.len() as u32;
        // SAFETY: both strings end in a zero and `len` is the buffer's size in characters
        let hr = unsafe {
            AssocQueryStringW(ASSOCF_NONE, ASSOCSTR_SHELLEXTENSION, PCWSTR(dotted.as_ptr()), PCWSTR(interface.as_ptr()), Some(windows::core::PWSTR(buf.as_mut_ptr())), &mut len)
        };
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        (hr.is_ok() && end > 0).then(|| String::from_utf16_lossy(&buf[..end]).to_uppercase())
    })
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
        Kind::ThumbnailOnly => unsafe { factory.GetImage(sz, SIIGBF(SIIGBF_THUMBNAILONLY.0 | SIIGBF_RESIZETOFIT.0 | SIIGBF_BIGGERSIZEOK.0))? },
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

/// How many pixels of a picture are kept (its longer side), for a request of `size`.
///
/// Up to three times what was asked: the askers that give their size in the interface's pixels count on a bigger picture to stay
/// sharp on a scaled display (three times covers 300 %, and leaves Windows' usual 256 px thumbnails alone for anything asked at
/// 86 px and up), but not on ANY size. The first time a video's thumbnail is made, Windows hands back the frame it took, 1280 x
/// 720 and 3.6 MB for a request of 144 px (measured on four new videos; asked a second time it gives 256 x 144). A screenful of
/// those filled the 64 MB the pictures may take, the ones in view were thrown out and asked for again, and the tiles went back to
/// their icons and to their pictures over and over until each had been asked for twice (a Discord report on 1.0.0: "thumbnails
/// ... keep switching back to the file icon").
fn kept_px(size: u32) -> u32 {
    size.saturating_mul(3)
}

/// A picture that came out bigger than its box on the screen, made to fit `size` (its longer side); any other is handed back as it
/// is. Done on the worker's thread.
fn fit_to(b: Bgra, size: u32) -> Bgra {
    let longest = b.width.max(b.height);
    if size == 0 || longest <= size || b.data.len() != (b.width as usize) * (b.height as usize) * 4 {
        return b;
    }
    let side = |v: u32| ((v as u64 * size as u64 + longest as u64 / 2) / longest as u64).max(1) as u32;
    let (w, h) = (side(b.width), side(b.height));
    let mut data = b.data;
    for p in data.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a != 255 {
            p[0] = ((p[0] as u32 * a + 127) / 255) as u8;
            p[1] = ((p[1] as u32 * a + 127) / 255) as u8;
            p[2] = ((p[2] as u32 * a + 127) / 255) as u8;
        }
    }
    let Some(src) = image::RgbaImage::from_raw(b.width, b.height, data) else { return Bgra { width: 0, height: 0, data: Vec::new() } };
    let mut data = image::imageops::resize(&src, w, h, image::imageops::FilterType::CatmullRom).into_raw();
    for p in data.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a != 0 && a != 255 {
            p[0] = ((p[0] as u32 * 255 + a / 2) / a).min(255) as u8;
            p[1] = ((p[1] as u32 * 255 + a / 2) / a).min(255) as u8;
            p[2] = ((p[2] as u32 * 255 + a / 2) / a).min(255) as u8;
        }
    }
    Bgra { width: w, height: h, data }
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
                            let image = get_image(&req.path, req.size, req.kind).ok().map(|b| fit_to(b, kept_px(req.size))).map(Arc::new);
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

    /// A type has a thumbnail provider only when the shell has one registered for it.
    #[test]
    fn a_type_has_a_thumbnail_provider_only_when_one_is_registered_for_it() {
        // (Windows registers one for its own picture types; nothing does for a type nobody knows, a name that is no extension, or none)
        assert!(super::has_provider("jpg"), "{:?}", super::provider_of("jpg"));
        assert!(!super::has_provider("rustyfiles-another-unknown-type"));
        assert!(!super::has_provider(""));
        assert!(!super::has_provider(r"a\b"));
    }

    /// What is kept of a picture: up to three times the asked size, so a video's first thumbnail does not stay at its frame's size.
    #[test]
    fn a_videos_first_thumbnail_does_not_stay_at_the_frames_size() {
        assert_eq!((kept_px(144), kept_px(96)), (432, 288));
        let frame = || Bgra { width: 1280, height: 720, data: vec![255; 1280 * 720 * 4] };
        let kept = fit_to(frame(), kept_px(144));
        assert_eq!((kept.width, kept.height), (432, 243), "a list that asked in the interface's pixels");
        // Windows' usual thumbnail for the same request is left as it is
        let usual = fit_to(Bgra { width: 256, height: 144, data: vec![9; 256 * 144 * 4] }, kept_px(144));
        assert_eq!((usual.width, usual.height, usual.data[0]), (256, 144, 9));
    }

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
