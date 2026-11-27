//! File categorization → symbol icon (Lucide) + type label.
//! Native shell icons/thumbnails come later via rf-win; this is the instant, zero-I/O fallback.

use crate::t;
use gpui_kit::assets::IconName as L;
use rf_core::Entry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Folder,
    Image,
    Video,
    Audio,
    Archive,
    Code,
    Document,
    Spreadsheet,
    Pdf,
    Executable,
    Text,
    Other,
}

pub fn category(e: &Entry) -> Category {
    if e.is_dir() {
        return Category::Folder;
    }
    category_for_ext(&e.ext)
}

pub fn category_for_ext(ext: &str) -> Category {
    use Category::*;
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "tif" | "tiff" | "ico" | "heic" | "heif" | "avif"
        | "svg" | "psd" | "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "jfif" | "exr" => Image,
        "mp4" | "mov" | "mkv" | "avi" | "wmv" | "webm" | "m4v" | "mpg" | "mpeg" | "flv" | "mxf" | "braw"
        | "r3d" | "ts" | "m2ts" | "mts" | "3gp" | "prproj" | "aep" | "drp" => Video,
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" | "opus" | "aiff" | "aif" | "mid" => Audio,
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "cab" | "iso" | "img" | "vhd"
        | "vhdx" => Archive,
        "rs" | "c" | "h" | "cpp" | "hpp" | "cs" | "js" | "jsx" | "ts_" | "tsx" | "py" | "go" | "java" | "kt"
        | "swift" | "rb" | "php" | "html" | "htm" | "css" | "scss" | "json" | "toml" | "yaml" | "yml"
        | "xml" | "sh" | "ps1" | "bat" | "cmd" | "lua" | "sql" | "vue" | "svelte" | "dart" | "zig" | "jsx_"
        | "mjs" | "cjs" | "ini" | "cfg" | "lock" => Code,
        "doc" | "docx" | "odt" | "rtf" | "pages" | "ppt" | "pptx" | "key" | "odp" => Document,
        "xls" | "xlsx" | "csv" | "ods" | "numbers" => Spreadsheet,
        "pdf" => Pdf,
        "exe" | "msi" | "appx" | "msix" | "dll" | "sys" | "lnk" | "url" | "com" | "scr" => Executable,
        "txt" | "md" | "log" | "nfo" | "srt" | "vtt" => Text,
        _ => Other,
    }
}

/// Files the shell can render a content thumbnail for: the types of our own list, and those of `may_have_thumbnail`.
pub fn has_thumbnail(e: &Entry) -> bool {
    has_sure_thumbnail(e) || may_have_thumbnail(e)
}

/// The types that are pictured as a matter of course: pictures, videos, PDFs and a few more.
pub fn has_sure_thumbnail(e: &Entry) -> bool {
    if e.is_dir() || e.is_cloud_placeholder() {
        return false;
    }
    matches!(category(e), Category::Image | Category::Video | Category::Pdf)
        || matches!(&*e.ext, "docx" | "pptx" | "xlsx" | "psd" | "blend" | "3mf" | "stl" | "glb")
        || rf_win::design::is_design_ext(&e.ext)
}

/// Any other type a thumbnail provider is registered for with Windows: a sound file's cover, a font's sample, and whatever an
/// installed thumbnail extension draws (SageThumbs 2K's types stayed icons). Such a file may or may not have a picture: one that
/// has none looks as it did before. Not programs and shortcuts: those keep their own icons (a shortcut's "thumbnail" would be its
/// target's).
pub fn may_have_thumbnail(e: &Entry) -> bool {
    !e.is_dir() && !e.is_cloud_placeholder() && !has_sure_thumbnail(e) && category(e) != Category::Executable && rf_win::thumbs::has_provider(&e.ext)
}

pub fn icon_for(cat: Category) -> L {
    use Category::*;
    match cat {
        Folder => L::Folder,
        Image => L::FileImage,
        Video => L::FileVideoCamera,
        Audio => L::FileMusic,
        Archive => L::FileArchive,
        Code => L::FileCode,
        Document => L::FileText,
        Spreadsheet => L::FileSpreadsheet,
        Pdf => L::FileText,
        Executable => L::AppWindow,
        Text => L::FileText,
        Other => L::File,
    }
}

pub fn type_label(e: &Entry) -> String {
    if e.is_dir() {
        return t!("Dosya klasörü").into();
    }
    match &*e.ext {
        "" => t!("Dosya").into(),
        "exe" => t!("Uygulama").into(),
        "lnk" => t!("Kısayol").into(),
        "txt" => t!("Metin Belgesi").into(),
        "pdf" => t!("PDF Belgesi").into(),
        "zip" => t!("Sıkıştırılmış (zip) klasör").into(),
        ext => format!("{} {}", ext.to_uppercase(), t!("Dosyası")),
    }
}
