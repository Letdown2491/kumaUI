//! Embedded icons and the extension-to-icon mapping. The SVGs come from
//! the vendored zed asset set (see icons/LICENSES); gpui renders them as
//! alpha masks tinted by the element's text color.

use std::borrow::Cow;
use std::path::Path;

use gpui::{AssetSource, SharedString};
use smallvec::SmallVec;

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "icons/archive.svg" => Some(include_bytes!("../icons/archive.svg")),
            "icons/arrow_left.svg" => Some(include_bytes!("../icons/arrow_left.svg")),
            "icons/arrow_right.svg" => Some(include_bytes!("../icons/arrow_right.svg")),
            "icons/arrow_up.svg" => Some(include_bytes!("../icons/arrow_up.svg")),
            "icons/chevron_down.svg" => Some(include_bytes!("../icons/chevron_down.svg")),
            "icons/chevron_right.svg" => Some(include_bytes!("../icons/chevron_right.svg")),
            "icons/chevron_up.svg" => Some(include_bytes!("../icons/chevron_up.svg")),
            "icons/clock.svg" => Some(include_bytes!("../icons/clock.svg")),
            "icons/eye.svg" => Some(include_bytes!("../icons/eye.svg")),
            "icons/eye_off.svg" => Some(include_bytes!("../icons/eye_off.svg")),
            "icons/lock.svg" => Some(include_bytes!("../icons/lock.svg")),
            "icons/lock_open.svg" => Some(include_bytes!("../icons/lock_open.svg")),
            "icons/file_code.svg" => Some(include_bytes!("../icons/file_code.svg")),
            "icons/file_doc.svg" => Some(include_bytes!("../icons/file_doc.svg")),
            "icons/file_generic.svg" => Some(include_bytes!("../icons/file_generic.svg")),
            "icons/file_text_filled.svg" => Some(include_bytes!("../icons/file_text_filled.svg")),
            "icons/folder.svg" => Some(include_bytes!("../icons/folder.svg")),
            "icons/folder_add.svg" => Some(include_bytes!("../icons/folder_add.svg")),
            "icons/grid.svg" => Some(include_bytes!("../icons/grid.svg")),
            "icons/list.svg" => Some(include_bytes!("../icons/list.svg")),
            "icons/square_plus.svg" => Some(include_bytes!("../icons/square_plus.svg")),
            "icons/image.svg" => Some(include_bytes!("../icons/image.svg")),
            "icons/trash.svg" => Some(include_bytes!("../icons/trash.svg")),
            _ => None,
        };
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

/// The tinted-ink color per icon kind: folders read warmer, plain files
/// dimmer, so the listing's rhythm comes from color, not just shape.
pub(crate) enum IconInk {
    Folder,
    Plain,
    Doc,
}

/// The icon path for a listing entry, by extension for files.
pub(crate) fn icon_path_for(name: &str, is_dir: bool) -> &'static str {
    if is_dir {
        return "icons/folder.svg";
    }
    let ext = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tiff" | "avif" => {
            "icons/image.svg"
        }
        "rs" | "py" | "js" | "ts" | "c" | "h" | "cpp" | "go" | "sh" | "fish" | "html" | "css"
        | "json" | "xml" | "yml" | "yaml" | "toml" => "icons/file_code.svg",
        "txt" | "md" | "log" | "conf" | "ini" | "csv" | "patch" | "diff" => {
            "icons/file_text_filled.svg"
        }
        "pdf" | "doc" | "docx" | "odt" | "xls" | "xlsx" | "ppt" | "pptx" | "ods" => {
            "icons/file_doc.svg"
        }
        "zip" | "tar" | "gz" | "xz" | "zst" | "bz2" | "7z" | "rar" | "iso" => "icons/archive.svg",
        _ => "icons/file_generic.svg",
    }
}

pub(crate) fn ink_for(path: &str) -> IconInk {
    match path {
        "icons/folder.svg" => IconInk::Folder,
        "icons/file_doc.svg" | "icons/file_text_filled.svg" => IconInk::Doc,
        _ => IconInk::Plain,
    }
}

/// Is this file name a decodable image (thumbnail candidate)?
pub(crate) fn is_image(name: &str) -> bool {
    let ext = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tiff" | "avif"
    )
}

/// Files that get a page-style thumbnail: plain images always, PDFs
/// when pdftocairo (poppler-utils) is available.
pub(crate) fn is_thumbable(name: &str) -> bool {
    is_image(name) || is_pdf(name)
}

/// PDF files: page 1 rendered by pdftocairo, falling back to the icon.
pub(crate) fn is_pdf(name: &str) -> bool {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase() == "pdf")
        .unwrap_or(false)
}

/// Decode a file into a BGRA `RenderImage` no larger than the bounds, the
/// byte order gpui expects. Same contract as kuma-shell's imaging module.
pub(crate) fn decode_thumbnail(path: &Path, max_width: u32, max_height: u32) -> Option<gpui::RenderImage> {
    let image = image::ImageReader::open(path).ok()?.decode().ok()?;
    Some(decode_to_render(image, max_width, max_height))
}

/// Render a PDF's first page to a thumbnail by shelling out to
/// pdftocairo (poppler-utils). None when the tool is missing or the
/// file is not a readable PDF, so callers fall back to the type icon.
pub(crate) fn decode_pdf_thumbnail(path: &Path, max: u32) -> Option<gpui::RenderImage> {
    let out = std::process::Command::new("pdftocairo")
        .args(["-png", "-f", "1", "-l", "1", "-singlefile", "-scale-to"])
        .arg(max.to_string())
        .arg(path)
        .arg("-")
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    let image = image::load_from_memory(&out.stdout).ok()?;
    Some(decode_to_render(image, max, max))
}

/// Convert a decoded image into the BGRA `RenderImage` gpui expects.
fn decode_to_render(image: image::DynamicImage, max_width: u32, max_height: u32) -> gpui::RenderImage {
    let mut thumb = image.thumbnail(max_width, max_height).to_rgba8();
    for pixel in thumb.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    gpui::RenderImage::new(SmallVec::from_buf([image::Frame::new(thumb)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_decode_fails_soft() {
        // a text file is not a pdf: no panic, just None
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(decode_pdf_thumbnail(&path, 256).is_none());
    }

    #[test]
    fn thumbable_gates() {
        assert!(is_thumbable("scan.pdf"));
        assert!(is_thumbable("SCAN.PDF"));
        assert!(!is_thumbable("notes.pdf.txt"));
        assert!(is_thumbable("photo.png"));
        assert!(!is_thumbable("report.docx"));
    }
}
