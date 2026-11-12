//! Hızlı Bakış resim çözücü: image paketi → resvg (SVG) → WIC (HEIC, AVIF, JXL, RAW…) → RAW'daki
//! gömülü JPEG → PSD birleşik görüntü → kabuk küçük resmi.

use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, anyhow};
use image::{AnimationDecoder, DynamicImage, ImageDecoder, RgbaImage};

use crate::thumbs::Bgra;

/// Çözülmüş resim: kareler (BGRA, süre ms), yön (EXIF 1–8), EXIF alanları.
pub struct Decoded {
    pub frames: Vec<(Bgra, u32)>,
    /// EXIF yönü (1 = düz). Kareler zaten bu yöne göre döndürülmüş gelir.
    pub orientation: u8,
    pub exif: Vec<(String, String)>,
    /// Asıl boyut (döndürülmüş, küçültülmemiş).
    pub width: u32,
    pub height: u32,
}

const RAW_EXTENSIONS: &[&str] =
    &["arw", "cr2", "cr3", "crw", "dng", "erf", "kdc", "mef", "mos", "mrw", "nef", "nrw", "orf", "pef", "raf", "raw", "rw2", "rwl", "sr2", "srf", "srw", "x3f"];

fn to_bgra(img: RgbaImage) -> Bgra {
    let (width, height) = img.dimensions();
    let mut data = img.into_raw();
    for px in data.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Bgra { width, height, data }
}

/// En uzun kenarı `max_side`'ı aşmayacak boyut.
fn fit(w: u32, h: u32, max_side: u32) -> (u32, u32) {
    let m = w.max(h);
    if m <= max_side || m == 0 {
        return (w, h);
    }
    let s = max_side as f64 / m as f64;
    (((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1))
}

fn shrink(img: RgbaImage, max_side: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let (nw, nh) = fit(w, h, max_side);
    if (nw, nh) == (w, h) { img } else { image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle) }
}

fn exif_fields(raw: &[u8]) -> (u8, Vec<(String, String)>) {
    let Ok(ex) = exif::Reader::new().read_raw(raw.to_vec()) else { return (1, Vec::new()) };
    let orientation = ex
        .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .map_or(1, |v| v.clamp(1, 8) as u8);
    let fields = ex
        .fields()
        .filter(|f| !matches!(f.tag, exif::Tag::MakerNote | exif::Tag::UserComment) && f.tag.description().is_some())
        .map(|f| (f.tag.to_string(), f.display_value().with_unit(&ex).to_string().trim_matches('"').to_string()))
        .filter(|(_, v)| !v.is_empty() && v.len() < 200)
        .collect();
    (orientation, fields)
}

fn orient(img: DynamicImage, o: u8) -> DynamicImage {
    match o {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

fn frames_of<'a>(frames: image::Frames<'a>, max_side: u32) -> anyhow::Result<Vec<(Bgra, u32)>> {
    let mut out = Vec::new();
    for f in frames {
        let f = f?;
        let (n, d) = f.delay().numer_denom_ms();
        let ms = if d == 0 { 100 } else { (n / d).max(20) };
        out.push((to_bgra(shrink(f.into_buffer(), max_side)), ms));
    }
    Ok(out)
}

/// Animasyonlu GIF / APNG / WebP: birden çok kare varsa onları döner.
fn animated(bytes: &[u8], format: image::ImageFormat, max_side: u32) -> Option<Vec<(Bgra, u32)>> {
    use image::codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder};
    let frames = match format {
        image::ImageFormat::Gif => frames_of(GifDecoder::new(Cursor::new(bytes)).ok()?.into_frames(), max_side).ok()?,
        image::ImageFormat::Png => {
            let d = PngDecoder::new(Cursor::new(bytes)).ok()?;
            if !d.is_apng().ok()? {
                return None;
            }
            frames_of(d.apng().ok()?.into_frames(), max_side).ok()?
        }
        image::ImageFormat::WebP => {
            let d = WebPDecoder::new(Cursor::new(bytes)).ok()?;
            if !d.has_animation() {
                return None;
            }
            frames_of(d.into_frames(), max_side).ok()?
        }
        _ => return None,
    };
    (frames.len() > 1).then_some(frames)
}

fn with_image_crate(bytes: &[u8], max_side: u32) -> anyhow::Result<Decoded> {
    let reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let format = reader.format().ok_or_else(|| anyhow!("unknown format"))?;
    if let Some(frames) = animated(bytes, format, max_side) {
        let (width, height) = (frames[0].0.width, frames[0].0.height);
        return Ok(Decoded { frames, orientation: 1, exif: Vec::new(), width, height });
    }
    let mut dec = reader.into_decoder()?;
    let (orientation, exif) = dec.exif_metadata().ok().flatten().map(|r| exif_fields(&r)).unwrap_or((1, Vec::new()));
    let img = orient(DynamicImage::from_decoder(dec)?, orientation);
    let (width, height) = (img.width(), img.height());
    Ok(Decoded { frames: vec![(to_bgra(shrink(img.into_rgba8(), max_side)), 0)], orientation, exif, width, height })
}

fn svg(bytes: &[u8], max_side: u32) -> anyhow::Result<Decoded> {
    use resvg::{tiny_skia, usvg};
    let mut opt = usvg::Options::default();
    // sistem yazı tipleri yalnız metin içeren SVG'de gerekir; yüklemesi pahalı, bir kez yapılır
    if bytes.windows(5).any(|w| w == b"<text") {
        static FONTS: std::sync::OnceLock<std::sync::Arc<usvg::fontdb::Database>> = std::sync::OnceLock::new();
        opt.fontdb = FONTS
            .get_or_init(|| {
                let mut db = usvg::fontdb::Database::new();
                db.load_system_fonts();
                std::sync::Arc::new(db)
            })
            .clone();
    }
    let tree = usvg::Tree::from_data(bytes, &opt)?;
    let size = tree.size();
    let (w, h) = (size.width().max(1.0), size.height().max(1.0));
    // küçük SVG'ler net görünsün diye en az 1024 piksele büyütülür
    let target = max_side.min(4096).max(1) as f32;
    let s = (target.min(w.max(h).max(1024.0)) / w.max(h)).max(0.01);
    let (pw, ph) = ((w * s).ceil() as u32, (h * s).ceil() as u32);
    let mut pix = tiny_skia::Pixmap::new(pw.max(1), ph.max(1)).ok_or_else(|| anyhow!("svg size"))?;
    resvg::render(&tree, tiny_skia::Transform::from_scale(s, s), &mut pix.as_mut());
    // tiny-skia önceden çarpılmış RGBA verir: düz alfaya çevir
    let mut data = pix.take();
    for px in data.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
        px.swap(0, 2);
    }
    Ok(Decoded { frames: vec![(Bgra { width: pw, height: ph, data }, 0)], orientation: 1, exif: Vec::new(), width: w.round() as u32, height: h.round() as u32 })
}

/// Windows Imaging Component: kurulu codec'ler (HEIC, AVIF, JXL, JPEG XR, kamera RAW…).
fn wic(path: &Path, max_side: u32) -> anyhow::Result<Decoded> {
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::core::{Interface, PCWSTR};
    crate::shell::ensure_com();
    let w = crate::wide::wide(path);
    unsafe {
        let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let dec = f.CreateDecoderFromFilename(PCWSTR(w.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)?;
        let frame = dec.GetFrame(0)?;
        let (mut fw, mut fh) = (0u32, 0u32);
        frame.GetSize(&mut fw, &mut fh)?;
        let (nw, nh) = fit(fw, fh, max_side);
        let mut src: IWICBitmapSource = frame.cast()?;
        if (nw, nh) != (fw, fh) {
            let sc = f.CreateBitmapScaler()?;
            sc.Initialize(&src, nw, nh, WICBitmapInterpolationModeFant)?;
            src = sc.cast()?;
        }
        let conv = f.CreateFormatConverter()?;
        conv.Initialize(&src, &GUID_WICPixelFormat32bppBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
        let stride = nw * 4;
        let mut data = vec![0u8; (stride * nh) as usize];
        conv.CopyPixels(std::ptr::null(), stride, &mut data)?;
        Ok(Decoded { frames: vec![(Bgra { width: nw, height: nh, data }, 0)], orientation: 1, exif: Vec::new(), width: fw, height: fh })
    }
}

/// Kamera RAW dosyasının içine gömülü en büyük JPEG önizleme.
fn raw_embedded_jpeg(bytes: &[u8], max_side: u32) -> anyhow::Result<Decoded> {
    let mut best: Option<(usize, u64)> = None;
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == 0xFF && bytes[i + 1] == 0xD8 && bytes[i + 2] == 0xFF {
            let r = image::ImageReader::with_format(Cursor::new(&bytes[i..]), image::ImageFormat::Jpeg);
            if let Ok((w, h)) = r.into_dimensions() {
                let area = w as u64 * h as u64;
                if best.is_none_or(|(_, a)| area > a) {
                    best = Some((i, area));
                }
            }
        }
        i += 1;
    }
    let (start, _) = best.context("no embedded JPEG")?;
    with_image_crate(&bytes[start..], max_side)
}

/// A design file's own picture (see `crate::design`) is small (CorelDRAW saves 256 px on the
/// short side): the previewer shows it enlarged smoothly to at least this long side.
const DESIGN_SIDE: u32 = 720;

/// The size a design file's picture is shown at: enlarged to `DESIGN_SIDE`, within `max_side`.
fn design_size(w: u32, h: u32, max_side: u32) -> (u32, u32) {
    let long = w.max(h).max(1);
    let target = DESIGN_SIDE.max(long).min(max_side);
    let s = target as f64 / long as f64;
    (((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1))
}

fn design(path: &Path, max_side: u32) -> Option<Decoded> {
    let img = crate::design::preview(path)?;
    let (w, h) = img.dimensions();
    let (dw, dh) = design_size(w, h, max_side);
    let img = if (dw, dh) == (w, h) { img } else { image::imageops::resize(&img, dw, dh, image::imageops::FilterType::CatmullRom) };
    Some(Decoded { frames: vec![(to_bgra(img), 0)], orientation: 1, exif: Vec::new(), width: dw, height: dh })
}

/// `img` in BGRA, its long side at most `max_side` (never enlarged).
pub(crate) fn bgra_within(img: RgbaImage, max_side: u32) -> Bgra {
    to_bgra(shrink(img, max_side))
}

fn psd(bytes: &[u8], max_side: u32) -> anyhow::Result<Decoded> {
    let p = psd::Psd::from_bytes(bytes).map_err(|e| anyhow!("psd: {e}"))?;
    let img = RgbaImage::from_raw(p.width(), p.height(), p.rgba()).context("psd pixels")?;
    let (width, height) = img.dimensions();
    Ok(Decoded { frames: vec![(to_bgra(shrink(img, max_side)), 0)], orientation: 1, exif: Vec::new(), width, height })
}

/// Resmin (EXIF yönüne göre döndürülmüş) boyutu; yalnız başlık okunur, hızlıdır.
pub fn dimensions(path: &Path) -> Option<(u32, u32)> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if crate::design::is_design_ext(&ext) {
        if let Some(img) = crate::design::preview(path) {
            return Some(design_size(img.width(), img.height(), u32::MAX));
        }
    }
    if ext == "svg" || ext == "svgz" {
        let bytes = std::fs::read(path).ok()?;
        let tree = resvg::usvg::Tree::from_data(&bytes, &resvg::usvg::Options::default()).ok()?;
        return Some((tree.size().width().round() as u32, tree.size().height().round() as u32));
    }
    if !RAW_EXTENSIONS.contains(&ext.as_str()) {
        if let Ok(r) = image::ImageReader::open(path).and_then(|r| r.with_guessed_format()) {
            if let Ok(mut d) = r.into_decoder() {
                let (w, h) = d.dimensions();
                let o = d.exif_metadata().ok().flatten().map_or(1, |raw| exif_fields(&raw).0);
                return Some(if (5..=8).contains(&o) { (h, w) } else { (w, h) });
            }
        }
    }
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::core::PCWSTR;
    crate::shell::ensure_com();
    let w = crate::wide::wide(path);
    unsafe {
        let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let dec = f.CreateDecoderFromFilename(PCWSTR(w.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand).ok()?;
        let frame = dec.GetFrame(0).ok()?;
        let (mut fw, mut fh) = (0u32, 0u32);
        frame.GetSize(&mut fw, &mut fh).ok()?;
        Some((fw, fh))
    }
}

/// Resmi tam çözünürlükte (en uzun kenar en çok `max_side`) çözer; sırayla dener.
pub fn decode_full(path: &Path, max_side: u32) -> anyhow::Result<Decoded> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    // (before reading the file: a CorelDRAW file is tens of MB, its picture one small entry)
    if crate::design::is_design_ext(&ext) {
        if let Some(d) = design(path, max_side) {
            return Ok(d);
        }
    }
    let bytes = std::fs::read(path).with_context(|| path.display().to_string())?;
    if ext == "svg" || ext == "svgz" {
        return svg(&bytes, max_side);
    }
    let raw = RAW_EXTENSIONS.contains(&ext.as_str());
    if !raw {
        if let Ok(d) = with_image_crate(&bytes, max_side) {
            return Ok(d);
        }
    }
    if let Ok(d) = wic(path, max_side) {
        return Ok(d);
    }
    if raw {
        if let Ok(d) = raw_embedded_jpeg(&bytes, max_side) {
            return Ok(d);
        }
    }
    if ext == "psd" || ext == "psb" {
        if let Ok(d) = psd(&bytes, max_side) {
            return Ok(d);
        }
    }
    let t = crate::thumbs::get_image(path, max_side.min(1024), crate::thumbs::Kind::Thumbnail)?;
    let (width, height) = (t.width, t.height);
    Ok(Decoded { frames: vec![(t, 0)], orientation: 1, exif: Vec::new(), width, height })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
    }

    #[test]
    fn decodes_common_formats() {
        for f in ["look/a.png", "look/a.jpg", "look/a.webp", "look/a.gif", "look/a.svg"] {
            let d = decode_full(&data(f), 4096).unwrap();
            assert!(d.frames[0].0.width > 0, "{f}");
        }
    }

    #[test]
    fn animated_gif_has_frames() {
        let d = decode_full(&data("look/anim.gif"), 4096).unwrap();
        assert_eq!(d.frames.len(), 3);
        assert_eq!(d.frames[0].1, 100);
    }

    #[test]
    fn exif_orientation_read() {
        let d = decode_full(&data("look/rot6.jpg"), 4096).unwrap();
        assert_eq!(d.orientation, 6);
        assert!(d.exif.iter().any(|(k, v)| k == "Model" && v.contains("Test Kamera")), "{:?}", d.exif);
    }

    #[test]
    fn large_images_are_scaled_down() {
        let d = decode_full(&data("look/a.png"), 32).unwrap();
        assert_eq!((d.frames[0].0.width, d.frames[0].0.height), (32, 24));
        assert_eq!((d.width, d.height), (64, 48));
    }

    #[test]
    fn header_dimensions() {
        assert_eq!(dimensions(&data("look/a.png")), Some((64, 48)));
        assert_eq!(dimensions(&data("look/rot6.jpg")), Some((40, 60)));
        assert_eq!(dimensions(&data("look/a.svg")), Some((80, 60)));
    }

    #[test]
    fn pixels_are_bgra() {
        let d = decode_full(&data("look/a.png"), 4096).unwrap();
        // sol üst köşe kırmızı (200, 30, 30) ama çapraz çizgi siyah; (60, 2) kırmızı
        let f = &d.frames[0].0;
        let i = ((2 * f.width + 60) * 4) as usize;
        assert_eq!(&f.data[i..i + 4], &[30, 30, 200, 255]);
    }
}
