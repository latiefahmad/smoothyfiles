//! PDF pages through Windows' own PDF engine (Windows.Data.Pdf, part of Windows 10 and 11):
//! no extra library. A page renders to PNG at the width asked for and is decoded to BGRA.
//!
//! WinRT calls block (`.get()`): use a worker thread, never the UI thread.

use std::path::Path;

use anyhow::{Context, anyhow};
use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
use windows::Storage::StorageFile;
use windows::Storage::Streams::{DataReader, InMemoryRandomAccessStream};
use windows::core::HSTRING;

use crate::thumbs::Bgra;

/// Widest page we render (physical pixels); wider requests are capped.
pub const MAX_WIDTH: u32 = 4096;

/// An open PDF.
pub struct Pdf {
    doc: PdfDocument,
}

/// Why a PDF can't be shown.
#[derive(Debug, PartialEq, Eq)]
pub enum PdfError {
    /// Protected with a password.
    Password,
    /// Not readable as a PDF (damaged, or not a PDF).
    Unreadable,
}

impl std::fmt::Display for PdfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PdfError::Password => "the PDF is protected with a password",
            PdfError::Unreadable => "the file can't be read as a PDF",
        })
    }
}

impl std::error::Error for PdfError {}

/// WinRT needs the calling thread in an apartment; a worker thread may have none yet.
fn ensure_apartment() {
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
    // S_FALSE / RPC_E_CHANGED_MODE (already initialized, maybe as STA) are fine for WinRT
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

impl Pdf {
    pub fn open(path: &Path, password: Option<&str>) -> anyhow::Result<Pdf> {
        ensure_apartment();
        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str()))
            .and_then(|op| op.join())
            .with_context(|| path.display().to_string())?;
        let opened = match password {
            Some(p) => PdfDocument::LoadFromFileWithPasswordAsync(&file, &HSTRING::from(p)).and_then(|op| op.join()),
            None => PdfDocument::LoadFromFileAsync(&file).and_then(|op| op.join()),
        };
        match opened {
            Ok(doc) => Ok(Pdf { doc }),
            // 0x8007052B ERROR_WRONG_PASSWORD
            Err(e) if e.code().0 as u32 == 0x8007_052B => Err(PdfError::Password.into()),
            Err(_) => Err(PdfError::Unreadable.into()),
        }
    }

    pub fn page_count(&self) -> u32 {
        self.doc.PageCount().unwrap_or(0)
    }

    /// Page size in DIPs (1/96 inch), as the page is shown (rotation applied).
    pub fn page_size(&self, index: u32) -> anyhow::Result<(f32, f32)> {
        let s = self.doc.GetPage(index)?.Size()?;
        Ok((s.Width, s.Height))
    }

    /// Renders page `index` (0-based) `width` pixels wide, on white.
    pub fn render(&self, index: u32, width: u32) -> anyhow::Result<Bgra> {
        let page = self.doc.GetPage(index)?;
        let stream = InMemoryRandomAccessStream::new()?;
        let opts = PdfPageRenderOptions::new()?;
        opts.SetDestinationWidth(width.clamp(16, MAX_WIDTH))?;
        page.RenderWithOptionsToStreamAsync(&stream, &opts)?.join()?;
        let size = u32::try_from(stream.Size()?).map_err(|_| anyhow!("page image too large"))?;
        let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0)?)?;
        reader.LoadAsync(size)?.join()?;
        let mut png = vec![0u8; size as usize];
        reader.ReadBytes(&mut png)?;
        let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png)?.into_rgba8();
        let (w, h) = img.dimensions();
        let mut data = img.into_raw();
        for px in data.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        Ok(Bgra { width: w, height: h, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-page PDF, 200 x 100 pt, with a filled rectangle.
    fn tiny_pdf(path: &Path) {
        let content = b"0 0 1 rg 20 20 160 60 re f";
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R >>".to_string(),
            format!("<< /Length {} >>\nstream\n{}\nendstream", content.len(), std::str::from_utf8(content).unwrap()),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", i + 1, o).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
        std::fs::write(path, out).unwrap();
    }

    #[test]
    fn renders_a_page() {
        let p = std::env::temp_dir().join(format!("rf-pdf-{}.pdf", std::process::id()));
        tiny_pdf(&p);
        let pdf = Pdf::open(&p, None).unwrap();
        assert_eq!(pdf.page_count(), 1);
        let (w, h) = pdf.page_size(0).unwrap();
        // 200 x 100 pt = 266.7 x 133.3 DIP
        assert!((w - 266.7).abs() < 1.0 && (h - 133.3).abs() < 1.0, "{w} x {h}");
        let img = pdf.render(0, 400).unwrap();
        assert_eq!((img.width, img.height), (400, 200));
        // the middle is the blue rectangle (BGRA), a corner is white
        let at = |x: u32, y: u32| &img.data[((y * img.width + x) * 4) as usize..][..3];
        assert_eq!(at(200, 100), &[255, 0, 0]);
        assert_eq!(at(2, 2), &[255, 255, 255]);
        let _ = std::fs::remove_file(&p);
        let bad = std::env::temp_dir().join(format!("rf-pdf-bad-{}.pdf", std::process::id()));
        std::fs::write(&bad, b"%PDF-1.4 bozuk").unwrap();
        let e = Pdf::open(&bad, None).err().unwrap();
        assert_eq!(e.downcast_ref::<PdfError>(), Some(&PdfError::Unreadable));
        let _ = std::fs::remove_file(&bad);
    }
}
