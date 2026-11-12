//! The picture a design program saves inside its file, for files Windows can't draw itself:
//! CorelDRAW, Illustrator, EPS, Figma, Krita, OpenRaster, Sketch, Adobe XD, XMind, Paint.NET,
//! Pixso, Procreate. The program writes it so that a file manager can show the design without
//! the program; we read only that picture (one ZIP entry, one TIFF section, one XMP packet), never
//! the drawing itself.
//!
//! Where each picture is was checked on real files (CorelDRAW 467 ZIP files: whole drawing in
//! `previews/thumbnail.png`, short side 256 px, while `previews/page1.png` is only the page and
//! blank in about half of them; EPS: the TIFF section of the DOS EPS header, XMP JPEG otherwise;
//! AI: XMP JPEG within the first 11 KB) or taken from the formats' published layouts.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use image::RgbaImage;

/// Design files whose embedded picture we show (lower case, no dot).
pub const EXTENSIONS: &[&str] = &["cdr", "cdrt", "ai", "eps", "epsf", "fig", "kra", "ora", "sketch", "xd", "xmind", "pdn", "pip", "pix", "procreate"];

/// DOS EPS: a binary header in front of the PostScript that points to a TIFF or WMF preview.
const DOS_EPS: [u8; 4] = [0xC5, 0xD0, 0xD3, 0xC6];
/// Most bytes read for one embedded picture (the largest seen: an 8.4 MB EPS TIFF).
const ENTRY_MAX: u64 = 48 << 20;
/// Most pixels decoded for one picture (a Krita canvas can be far bigger: then its preview is used).
const PIXELS_MAX: u64 = 64 << 20;
/// How far into an AI / EPS file the XMP thumbnail is looked for (found within 11 KB in every
/// Illustrator file checked; plain EPS keep it near the start too).
const XMP_SCAN: usize = 1 << 20;

pub fn is_design_ext(ext: &str) -> bool {
    EXTENSIONS.contains(&ext)
}

fn ext_of(path: &Path) -> String {
    path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    find(hay, needle, 0).is_some()
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

/// Whether a file with extension `ext` whose first bytes are `head` carries a picture we can
/// show, so a viewer can decide without reading more. Illustrator files that are PDFs are left
/// to the PDF viewer (it draws the artboard itself, sharper than any thumbnail); a macOS
/// `._name` sidecar (00 05 16 07) is never taken for the design it is named after.
pub fn sniff(ext: &str, head: &[u8]) -> bool {
    match ext {
        "ai" => head.starts_with(b"%!PS") && contains(head, b"xmpGImg:image"),
        "eps" | "epsf" => head.starts_with(&DOS_EPS) || (head.starts_with(b"%!PS") && contains(head, b"xmpGImg:image")),
        "pdn" => head.starts_with(b"PDN"),
        // up to CorelDRAW X3 a RIFF file ("RIFF", size, "CDR" + version letter)
        "cdr" if head.starts_with(b"RIFF") && head.get(8..11) == Some(b"CDR") => true,
        e if is_design_ext(e) => head.starts_with(b"PK\x03\x04"),
        _ => false,
    }
}

/// The ZIP entries that hold a picture, best first: an exact name (any letter case), or a name
/// ending in the given text when it starts with `*`.
fn zip_candidates(ext: &str) -> &'static [&'static str] {
    match ext {
        // the whole drawing (the pasteboard too); the X5–X7 bitmap; the page alone, often blank
        "cdr" | "cdrt" => &["previews/thumbnail.png", "metadata/thumbnails/thumbnail.bmp", "previews/page1.png", "metadata/thumbnails/page1.bmp"],
        "fig" => &["thumbnail.png"],
        // the flattened canvas, then the small preview every version writes
        "kra" => &["mergedimage.png", "preview.png"],
        "ora" => &["mergedimage.png", "Thumbnails/thumbnail.png"],
        "sketch" => &["previews/preview.png"],
        "xd" => &["preview.png", "thumbnail.png"],
        "xmind" => &["Thumbnails/thumbnail.png"],
        "pip" | "pix" => &["*.thumb.png"],
        "procreate" => &["*QuickLook/Thumbnail.png"],
        _ => &[],
    }
}

/// Page pictures that are often an empty sheet (the design sits beside the page): skipped then.
fn may_be_blank(entry: &str) -> bool {
    entry.contains("page")
}

fn is_blank(img: &RgbaImage) -> bool {
    img.pixels().all(|p| p.0[3] < 8 || (p.0[0] >= 250 && p.0[1] >= 250 && p.0[2] >= 250))
}

fn decode(bytes: &[u8]) -> Option<RgbaImage> {
    let mut r = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(PIXELS_MAX * 4);
    r.limits(limits);
    let (w, h) = r.into_dimensions().ok()?;
    if w as u64 * h as u64 > PIXELS_MAX {
        return None;
    }
    let mut r = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(PIXELS_MAX * 4);
    r.limits(limits);
    Some(r.decode().ok()?.to_rgba8())
}

fn zip_preview(file: File, ext: &str) -> Option<RgbaImage> {
    // the central directory only: a CorelDRAW file can unpack to gigabytes, one entry is read
    let mut zip = zip::ZipArchive::new(BufReader::new(file)).ok()?;
    let names: Vec<String> = zip.file_names().filter_map(|n| n.ok().map(|n| n.into_owned())).collect();
    for want in zip_candidates(ext) {
        let hit = |n: &&String| {
            let n = n.replace('\\', "/");
            match want.strip_prefix('*') {
                Some(tail) => n.to_ascii_lowercase().ends_with(&tail.to_ascii_lowercase()),
                None => n.eq_ignore_ascii_case(want),
            }
        };
        let Some(name) = names.iter().find(hit) else { continue };
        let Ok(mut entry) = zip.by_name(name) else { continue };
        if entry.size() > ENTRY_MAX {
            continue;
        }
        // read at most the cap, whatever the header claims
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        if (&mut entry).take(ENTRY_MAX + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > ENTRY_MAX {
            continue;
        }
        drop(entry);
        let Some(img) = decode(&bytes) else { continue };
        if may_be_blank(want) && is_blank(&img) {
            continue;
        }
        return Some(img);
    }
    None
}

fn u32_le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// DOS EPS: the TIFF section (Illustrator and Photoshop write one), else the XMP thumbnail of the
/// PostScript section.
fn dos_eps(mut file: File) -> Option<RgbaImage> {
    let len = file.metadata().ok()?.len();
    let mut h = [0u8; 30];
    file.read_exact(&mut h).ok()?;
    let (ps_at, ps_len, tiff_at, tiff_len) = (u32_le(&h, 4) as u64, u32_le(&h, 8) as u64, u32_le(&h, 20) as u64, u32_le(&h, 24) as u64);
    if tiff_len > 0 && tiff_len <= ENTRY_MAX && tiff_at + tiff_len <= len {
        let mut tiff = vec![0u8; tiff_len as usize];
        if file.seek(SeekFrom::Start(tiff_at)).is_ok() && file.read_exact(&mut tiff).is_ok() {
            if let Some(img) = decode(&tiff).or_else(|| palette_tiff(&tiff)) {
                return Some(img);
            }
        }
    }
    if ps_len > 0 && ps_at < len {
        let mut ps = Vec::new();
        file.seek(SeekFrom::Start(ps_at)).ok()?;
        (&mut file).take(ps_len.min(XMP_SCAN as u64)).read_to_end(&mut ps).ok()?;
        return xmp_thumbnail(&ps);
    }
    None
}

/// Uncompressed 8-bit palette TIFF, with or without an alpha sample: what Photoshop puts in an
/// EPS as its preview, and what the image crate does not read.
fn palette_tiff(b: &[u8]) -> Option<RgbaImage> {
    let le = match b.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |o: usize| -> Option<u32> {
        let s = b.get(o..o + 2)?;
        Some(if le { u16::from_le_bytes([s[0], s[1]]) } else { u16::from_be_bytes([s[0], s[1]]) } as u32)
    };
    let u32_at = |o: usize| -> Option<u32> {
        let s = b.get(o..o + 4)?;
        Some(if le { u32::from_le_bytes([s[0], s[1], s[2], s[3]]) } else { u32::from_be_bytes([s[0], s[1], s[2], s[3]]) })
    };
    if u16_at(2)? != 42 {
        return None;
    }
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    // a tag's values: SHORT (3) or LONG (4), inline when they fit in 4 bytes
    let values = |entry: usize| -> Option<Vec<u32>> {
        let (ty, n) = (u16_at(entry + 2)?, u32_at(entry + 4)? as usize);
        let size = match ty {
            3 => 2,
            4 => 4,
            _ => return None,
        };
        if n > 1 << 20 {
            return None;
        }
        let at = if n * size <= 4 { entry + 8 } else { u32_at(entry + 8)? as usize };
        (0..n).map(|i| if size == 2 { u16_at(at + i * 2) } else { u32_at(at + i * 4) }).collect()
    };
    let (mut w, mut h, mut bits, mut comp, mut photo, mut spp) = (0, 0, 8, 1, 0, 1);
    let (mut offsets, mut counts, mut map) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..count {
        let e = ifd + 2 + i * 12;
        let tag = u16_at(e)?;
        let v = || values(e).unwrap_or_default();
        let first = || v().first().copied().unwrap_or(0);
        match tag {
            256 => w = first(),
            257 => h = first(),
            258 => bits = first(),
            259 => comp = first(),
            262 => photo = first(),
            273 => offsets = v(),
            277 => spp = first(),
            279 => counts = v(),
            320 => map = v(),
            _ => {}
        }
    }
    if comp != 1 || photo != 3 || bits != 8 || !(1..=2).contains(&spp) || map.len() < 3 * 256 || w == 0 || h == 0 {
        return None;
    }
    if w as u64 * h as u64 > PIXELS_MAX || offsets.len() != counts.len() {
        return None;
    }
    let mut data = Vec::with_capacity((w * h * spp) as usize);
    for (&o, &n) in offsets.iter().zip(&counts) {
        data.extend_from_slice(b.get(o as usize..(o as usize).checked_add(n as usize)?)?);
    }
    let need = (w * h * spp) as usize;
    if data.len() < need {
        return None;
    }
    let mut img = RgbaImage::new(w, h);
    for (i, px) in img.pixels_mut().enumerate() {
        let s = &data[i * spp as usize..];
        let k = s[0] as usize;
        let c = |n: usize| (map[n * 256 + k] >> 8) as u8;
        px.0 = [c(0), c(1), c(2), if spp == 2 { s[1] } else { 255 }];
    }
    Some(img)
}

/// The JPEG thumbnail Adobe programs write into a file's XMP metadata (`xmpGImg:image`, base64).
fn xmp_thumbnail(bytes: &[u8]) -> Option<RgbaImage> {
    let start = find(bytes, b"<xmpGImg:image>", 0)? + b"<xmpGImg:image>".len();
    let end = find(bytes, b"</xmpGImg:image>", start)?;
    let text = String::from_utf8_lossy(&bytes[start..end]).replace("&#xA;", "").replace("&#xD;", "");
    decode(&base64(&text)?)
}

/// Standard base64; whitespace is skipped.
fn base64(text: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut n) = (0u32, 0);
    for c in text.bytes() {
        if c.is_ascii_whitespace() {
            continue;
        }
        if c == b'=' {
            break;
        }
        acc = (acc << 6) | val(c)?;
        n += 1;
        if n == 4 {
            out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8, acc as u8]);
            (acc, n) = (0, 0);
        }
    }
    match n {
        2 => out.push((acc >> 4) as u8),
        3 => out.extend_from_slice(&[(acc >> 10) as u8, (acc >> 2) as u8]),
        _ => {}
    }
    Some(out)
}

/// Paint.NET: the header's XML carries the thumbnail as `<thumb png="base64">`.
fn pdn(mut file: File) -> Option<RgbaImage> {
    let mut head = Vec::new();
    (&mut file).take(XMP_SCAN as u64).read_to_end(&mut head).ok()?;
    let t = find(&head, b"<thumb", 0)?;
    let start = find(&head, b"png=\"", t)? + 5;
    let end = find(&head, b"\"", start)?;
    decode(&base64(&String::from_utf8_lossy(&head[start..end]))?)
}

/// CorelDRAW up to X3 (a RIFF file, "CDR" + version letter): a top-level `DISP` chunk holds the
/// preview as a clipboard bitmap (u32 format 8 = CF_DIB, then the DIB). Checked on files from
/// CorelDRAW 9 to X3.
fn riff_cdr(mut file: File) -> Option<RgbaImage> {
    let len = file.metadata().ok()?.len();
    let mut at = 12u64;
    for _ in 0..64 {
        let mut h = [0u8; 8];
        file.seek(SeekFrom::Start(at)).ok()?;
        file.read_exact(&mut h).ok()?;
        let size = u32_le(&h, 4) as u64;
        if &h[0..4] == b"DISP" {
            if size < 4 + 40 || size > ENTRY_MAX || at + 8 + size > len {
                return None;
            }
            let mut d = vec![0u8; size as usize];
            file.read_exact(&mut d).ok()?;
            if u32_le(&d, 0) != 8 {
                return None;
            }
            return dib(&d[4..]);
        }
        at += 8 + size + (size & 1);
        if at >= len {
            return None;
        }
    }
    None
}

/// A headerless Windows bitmap (BITMAPINFOHEADER, palette, pixels): given a BMP file header so
/// the image crate reads it.
fn dib(d: &[u8]) -> Option<RgbaImage> {
    let header = u32_le(d, 0) as usize;
    if header < 40 || d.len() < header {
        return None;
    }
    let bits = u16::from_le_bytes([d[14], d[15]]) as usize;
    let compression = u32_le(d, 16);
    let used = u32_le(d, 32) as usize;
    let colors = if used > 0 { used } else if bits <= 8 { 1 << bits } else { 0 };
    let masks = if compression == 3 && header == 40 { 12 } else { 0 };
    let pixels_at = 14 + header + masks + colors * 4;
    let mut bmp = Vec::with_capacity(14 + d.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((14 + d.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0, 0, 0, 0]);
    bmp.extend_from_slice(&(pixels_at as u32).to_le_bytes());
    bmp.extend_from_slice(d);
    decode(&bmp)
}

fn read_preview(path: &Path) -> Option<RgbaImage> {
    let ext = ext_of(path);
    if !is_design_ext(&ext) {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let mut head = [0u8; 12];
    file.read_exact(&mut head).ok()?;
    file.seek(SeekFrom::Start(0)).ok()?;
    if head.starts_with(b"PK\x03\x04") {
        return zip_preview(file, &ext);
    }
    if head.starts_with(b"RIFF") && &head[8..11] == b"CDR" {
        return riff_cdr(file);
    }
    if head.starts_with(&DOS_EPS) {
        return dos_eps(file);
    }
    if head.starts_with(b"PDN") {
        return pdn(file);
    }
    if head.starts_with(b"%!PS") || head.starts_with(b"%PDF") {
        let mut start = Vec::new();
        file.take(XMP_SCAN as u64).read_to_end(&mut start).ok()?;
        return xmp_thumbnail(&start);
    }
    None
}

/// The last picture read: a viewer asks for the size first and the picture right after.
static LAST: std::sync::Mutex<Option<(PathBuf, u64, Option<SystemTime>, RgbaImage)>> = std::sync::Mutex::new(None);

/// The picture saved inside `path`, at its own size; `None` when the file has none we can read
/// (or is not a design file).
pub fn preview(path: &Path) -> Option<RgbaImage> {
    let meta = std::fs::metadata(path).ok()?;
    let key = (path.to_path_buf(), meta.len(), meta.modified().ok());
    if let Some((p, l, m, img)) = LAST.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if (p, *l, *m) == (&key.0, key.1, key.2) {
            return Some(img.clone());
        }
    }
    let img = read_preview(path)?;
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some((key.0, key.1, key.2, img.clone()));
    Some(img)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rf-design-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn png(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let img = RgbaImage::from_pixel(w, h, image::Rgba([rgb[0], rgb[1], rgb[2], 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    fn zip_file(path: &Path, entries: &[(&str, Vec<u8>)]) {
        let mut z = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, data) in entries {
            z.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(data).unwrap();
        }
        z.finish().unwrap();
    }

    /// CorelDRAW: the whole drawing, not the page; a blank page is passed over.
    #[test]
    fn coreldraw_shows_the_drawing() {
        let p = temp("a.cdr");
        zip_file(&p, &[("mimetype", b"application/x-vnd.corel.zcf.draw.document+zip".to_vec()), ("previews/page1.png", png(18, 25, [255, 255, 255])), ("previews/thumbnail.png", png(34, 25, [200, 30, 30]))]);
        let img = preview(&p).expect("picture");
        assert_eq!(img.dimensions(), (34, 25));
        assert_eq!(img.get_pixel(3, 3).0, [200, 30, 30, 255]);
        // no thumbnail and a blank page: nothing (the icon stays)
        let q = temp("b.cdr");
        zip_file(&q, &[("previews/page1.png", png(18, 25, [255, 255, 255]))]);
        assert!(preview(&q).is_none());
        // no thumbnail, a page with something on it: the page
        let r = temp("c.cdr");
        zip_file(&r, &[("previews/page1.png", png(18, 25, [10, 10, 10]))]);
        assert_eq!(preview(&r).map(|i| i.dimensions()), Some((18, 25)));
    }

    #[test]
    fn other_zip_formats() {
        let p = temp("a.kra");
        zip_file(&p, &[("preview.png", png(8, 8, [1, 2, 3])), ("mergedimage.png", png(40, 30, [9, 9, 9]))]);
        assert_eq!(preview(&p).map(|i| i.dimensions()), Some((40, 30)));
        let q = temp("a.procreate");
        zip_file(&q, &[("Art/QuickLook/Thumbnail.png", png(12, 16, [9, 9, 9]))]);
        assert_eq!(preview(&q).map(|i| i.dimensions()), Some((12, 16)));
    }

    #[test]
    fn xmp_thumbnail_in_postscript() {
        let jpeg = {
            let mut out = Vec::new();
            image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(20, 10, image::Rgb([0, 128, 255])))
                .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg)
                .unwrap();
            out
        };
        let b64 = {
            const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut s = String::new();
            for (i, c) in jpeg.chunks(3).enumerate() {
                let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
                for k in 0..4 {
                    s.push(if k <= c.len() { T[(n >> (18 - 6 * k) & 63) as usize] as char } else { '=' });
                }
                if i % 19 == 18 {
                    s.push_str("&#xA;");
                }
            }
            s
        };
        let p = temp("a.eps");
        std::fs::write(&p, format!("%!PS-Adobe-3.1 EPSF-3.0\r<xmpGImg:image>{b64}</xmpGImg:image>\r%%EOF")).unwrap();
        assert!(sniff("eps", &std::fs::read(&p).unwrap()));
        assert_eq!(preview(&p).map(|i| i.dimensions()), Some((20, 10)));
    }

    #[test]
    fn dos_eps_palette_tiff() {
        // a 2×1 palette TIFF with alpha (index, alpha per pixel), as Photoshop writes it
        let mut t = Vec::new();
        t.extend_from_slice(b"II*\0");
        let ifd_at = 8 + 4 + 1536;
        t.extend_from_slice(&(ifd_at as u32).to_le_bytes());
        t.extend_from_slice(&[1, 255, 2, 128]); // pixel data at 8
        let mut map = vec![0u16; 768];
        map[1] = 0xFF00; // index 1: red
        map[256 + 2] = 0xFF00; // index 2: green
        for v in &map {
            t.extend_from_slice(&v.to_le_bytes());
        }
        let tags: [(u16, u16, u32, u32); 9] =
            [(256, 3, 1, 2), (257, 3, 1, 1), (258, 3, 1, 8), (259, 3, 1, 1), (262, 3, 1, 3), (273, 4, 1, 8), (277, 3, 1, 2), (279, 4, 1, 4), (320, 3, 768, 12)];
        t.extend_from_slice(&(tags.len() as u16).to_le_bytes());
        for (tag, ty, n, v) in tags {
            t.extend_from_slice(&tag.to_le_bytes());
            t.extend_from_slice(&ty.to_le_bytes());
            t.extend_from_slice(&n.to_le_bytes());
            t.extend_from_slice(&v.to_le_bytes());
        }
        t.extend_from_slice(&0u32.to_le_bytes());
        let ps = b"%!PS-Adobe-3.0 EPSF-3.0\n%%EOF\n";
        let mut f = Vec::new();
        f.extend_from_slice(&DOS_EPS);
        for v in [32u32, ps.len() as u32, 0, 0, 32 + ps.len() as u32, t.len() as u32] {
            f.extend_from_slice(&v.to_le_bytes());
        }
        f.extend_from_slice(&[0xFF, 0xFF, 0, 0]);
        f.extend_from_slice(ps);
        f.extend_from_slice(&t);
        let p = temp("b.eps");
        std::fs::write(&p, &f).unwrap();
        let img = preview(&p).expect("picture");
        assert_eq!(img.dimensions(), (2, 1));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 255, 0, 128]);
    }

    #[test]
    fn sniffing() {
        assert!(sniff("cdr", b"PK\x03\x04rest"));
        // a macOS sidecar named like the design
        assert!(!sniff("cdr", &[0, 5, 0x16, 7, 0, 2, 0, 0]));
        // Illustrator files that are PDFs go to the PDF viewer
        assert!(!sniff("ai", b"%PDF-1.6\n%"));
        assert!(sniff("eps", &[0xC5, 0xD0, 0xD3, 0xC6, 32, 0]));
        assert!(!sniff("eps", b"%!PS-Adobe-3.0 EPSF-3.0\n%%BoundingBox"));
        assert!(!sniff("png", b"PK\x03\x04"));
    }

    #[test]
    fn base64_decodes() {
        assert_eq!(base64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64("aGVs\r\nbG8h").unwrap(), b"hello!");
        assert!(base64("a*b").is_none());
    }
}
