//! Görüntüleyiciler. Öncelikler QuickLook'unkiler: Resim 0, Video −3, Arşiv −5, Metin −5,
//! Bilgi kartı en son. Eşit öncelikte kayıt sırası geçerli (arşiv metinden önce).

pub mod archive;
pub mod csv;
pub mod email;
pub mod font;
pub mod hex;
pub mod image;
pub mod info;
pub mod markdown;
pub mod pdf;
pub mod rd_video;
pub mod text;
pub mod video;
pub mod zoom;

use super::viewer::Viewer;

pub fn all() -> Vec<Box<dyn Viewer>> {
    vec![
        Box::new(image::ImageViewer::default()),
        Box::new(video::VideoViewer::default()),
        Box::new(pdf::PdfViewer::default()),
        Box::new(archive::ArchiveViewer::default()),
        Box::new(markdown::MarkdownViewer::default()),
        Box::new(csv::CsvViewer::default()),
        Box::new(email::EmailViewer::default()),
        Box::new(font::FontViewer::default()),
        Box::new(hex::HexViewer::default()),
        Box::new(text::TextViewer::default()),
    ]
}

pub(crate) fn file_name(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string_lossy().into_owned())
}
