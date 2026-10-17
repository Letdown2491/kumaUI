//! Embedded icons and the extension-to-icon mapping. The SVGs come from
//! the vendored zed asset set (see icons/LICENSES); gpui renders them as
//! alpha masks tinted by the element's text color.

use std::borrow::Cow;
use std::fs;
use std::path::Path;
use std::sync::{Arc, LazyLock};

use gpui::{AssetSource, SharedString};

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "icons/archive.svg" => Some(include_bytes!("../icons/archive.svg")),
            "icons/arrow_left.svg" => Some(include_bytes!("../icons/arrow_left.svg")),
            "icons/arrow_right.svg" => Some(include_bytes!("../icons/arrow_right.svg")),
            "icons/arrow_up.svg" => Some(include_bytes!("../icons/arrow_up.svg")),
            // the titlebar's window controls (family-made, same bytes
            // as the monitor's set)
            "icons/titlebar_minimize.svg" => {
                Some(include_bytes!("../icons/titlebar_minimize.svg"))
            }
            "icons/titlebar_maximize.svg" => {
                Some(include_bytes!("../icons/titlebar_maximize.svg"))
            }
            "icons/titlebar_close.svg" => Some(include_bytes!("../icons/titlebar_close.svg")),
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
            "icons/plus.svg" => Some(include_bytes!("../icons/plus.svg")),
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

/// SVG files: rasterized in-process for the grid thumbs, the Quick
/// Look pane, and the viewer. SVGs are resolution independent, so a
/// small icon's thumb still renders crisp at the fit size.
pub(crate) fn is_svg(name: &str) -> bool {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase() == "svg")
        .unwrap_or(false)
}

/// Thumbable grid entries: images always, PDFs when pdftocairo
/// (poppler-utils) is available, SVGs through the in-process
/// rasterizer, and videos (a video's grid thumb is its poster
/// frame; playback stays with the default handler). Decodes run
/// through the bounded thumb pool.
pub(crate) fn is_thumbable(name: &str) -> bool {
    is_image(name) || is_pdf(name) || is_svg(name) || is_video(name)
}

/// EPUB ebooks: the Quick Look book glance (cover plus metadata).
pub(crate) fn is_epub(name: &str) -> bool {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase() == "epub")
        .unwrap_or(false)
}

/// PDF files: a page rendered by pdftocairo, falling back to the icon.
pub(crate) fn is_pdf(name: &str) -> bool {    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase() == "pdf")
        .unwrap_or(false)
}

/// Video files: a poster frame for the Quick Look pane and the grid
/// thumbs (through the bounded pool). Playback itself stays with the
/// system default handler (gpui has no decoder).
pub(crate) fn is_video(name: &str) -> bool {
    let ext = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(ext.as_str(), "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v")
}

/// Decode a file into a BGRA `RenderImage` no larger than the bounds, the
/// byte order gpui expects. Same contract as kuma-shell's imaging module.
pub(crate) fn decode_thumbnail(path: &Path, max_width: u32, max_height: u32) -> Option<gpui::RenderImage> {
    kuma_platform::imaging::decode_fit(path, max_width, max_height)
}

/// An image file decoded into a plain `DynamicImage`, uncapped and
/// unswapped: the grid thumb job's intermediate, so the disk cache
/// can encode PNGs of the pre-swap pixels.
pub(crate) fn decode_thumbnail_dynamic(path: &Path) -> Option<image::DynamicImage> {
    image::ImageReader::open(path).ok()?.decode().ok()
}

/// An epub's cover bytes into the BGRA `RenderImage`, same contract
/// as the other decoders.
pub(crate) fn decode_cover(bytes: &[u8], max: u32) -> Option<gpui::RenderImage> {
    let image = image::load_from_memory(bytes).ok()?;
    Some(kuma_platform::imaging::fit(image, max, max))
}

/// The epub cover's dynamic twin, same split as images.
pub(crate) fn decode_cover_dynamic(bytes: &[u8]) -> Option<image::DynamicImage> {
    image::load_from_memory(bytes).ok()
}

/// Render one PDF page to a thumbnail by shelling out to pdftocairo
/// (poppler-utils). None when the tool is missing or the file is not
/// a readable PDF, so callers fall back to the type icon.
pub(crate) fn decode_pdf_thumbnail(
    path: &Path,
    page: usize,
    max: u32,
) -> Option<gpui::RenderImage> {
    decode_pdf_dynamic(path, page, max).map(|image| kuma_platform::imaging::fit(image, max, max))
}

/// The pdf thumb's dynamic twin: raw pdftocairo PNG bytes decoded,
/// pre-swap, for the grid thumb job's disk cache.
pub(crate) fn decode_pdf_dynamic(
    path: &Path,
    page: usize,
    max: u32,
) -> Option<image::DynamicImage> {
    let page = page.to_string();
    let out = std::process::Command::new("pdftocairo")
        .args(["-png", "-f", &page, "-l", &page, "-singlefile", "-scale-to"])
        .arg(max.to_string())
        .arg(path)
        .arg("-")
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    image::load_from_memory(&out.stdout).ok()
}

/// The SVG preview's font database: system fonts scanned once per
/// process (the scan is the costly part). fontdb's generic families
/// default to Microsoft names kumaOS does not ship, so each generic
/// that resolves to nothing points at a face that exists: the same
/// repair gpui's svg_renderer makes for its bundled faces.
static SVG_FONTDB: LazyLock<Arc<usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut db = usvg::fontdb::Database::new();
    db.load_system_fonts();
    let fallback = db
        .faces()
        .next()
        .and_then(|face| face.families.first().map(|(name, _)| name.clone()));
    if let Some(fallback) = fallback {
        for family in [
            usvg::fontdb::Family::SansSerif,
            usvg::fontdb::Family::Serif,
            usvg::fontdb::Family::Monospace,
            usvg::fontdb::Family::Cursive,
            usvg::fontdb::Family::Fantasy,
        ] {
            let query = usvg::fontdb::Query {
                families: &[family],
                ..Default::default()
            };
            if db.query(&query).is_none() {
                match family {
                    usvg::fontdb::Family::SansSerif => db.set_sans_serif_family(&fallback),
                    usvg::fontdb::Family::Serif => db.set_serif_family(&fallback),
                    usvg::fontdb::Family::Monospace => db.set_monospace_family(&fallback),
                    usvg::fontdb::Family::Cursive => db.set_cursive_family(&fallback),
                    usvg::fontdb::Family::Fantasy => db.set_fantasy_family(&fallback),
                    _ => {}
                }
            }
        }
    }
    Arc::new(db)
});

/// usvg's parse options: the shared system fontdb under the default
/// font matching, the same shape gpui's svg_renderer builds. A fresh
/// options per decode (cheap boxed closures); the database is the
/// process-wide part.
fn svg_options() -> usvg::Options<'static> {
    let default_font_selector = usvg::FontResolver::default_font_selector();
    let default_fallback_selector = usvg::FontResolver::default_fallback_selector();
    usvg::Options {
        font_resolver: usvg::FontResolver {
            select_font: Box::new(move |font, db| {
                if db.is_empty() {
                    *db = SVG_FONTDB.clone();
                }
                default_font_selector(font, db)
            }),
            select_fallback: Box::new(move |ch, fonts, db| {
                default_fallback_selector(ch, fonts, db)
            }),
        },
        ..Default::default()
    }
}

/// Rasterize an SVG file into a straight-alpha RGBA `DynamicImage`
/// fitted within `max x max`, the thumb pipeline's shared
/// intermediate. resvg executes nothing and fetches nothing, so an
/// untrusted file is on par with an untrusted raster here.
pub(crate) fn decode_svg_dynamic(path: &Path, max: u32) -> Option<image::DynamicImage> {
    let bytes = fs::read(path).ok()?;
    decode_svg_bytes_dynamic(&bytes, max)
}

/// The svg decode from raw bytes, the test surface of the same
/// contract.
pub(crate) fn decode_svg_bytes_dynamic(bytes: &[u8], max: u32) -> Option<image::DynamicImage> {
    // a pathological file stays sane: cap the bytes handed to the
    // parser the way the epub cover is capped
    (bytes.len() <= 16 * 1024 * 1024).then_some(bytes)?;
    let tree = usvg::Tree::from_data(bytes, &svg_options()).ok()?;
    let size = tree.size();
    // fit within max on the long side, up or down: SVGs are
    // resolution independent, so a small icon's thumb upscales crisp
    let scale = max as f32 / size.width().max(size.height()).max(1.0);
    let width = (size.width() * scale).round().max(1.0) as u32;
    let height = (size.height() * scale).round().max(1.0) as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(
            width as f32 / size.width(),
            height as f32 / size.height(),
        ),
        &mut pixmap.as_mut(),
    );
    demultiply(pixmap)
}

/// tiny-skia's pixmap is premultiplied RGBA; the image pipeline's
/// DynamicImage wants straight alpha.
fn demultiply(pixmap: resvg::tiny_skia::Pixmap) -> Option<image::DynamicImage> {
    let (width, height) = (pixmap.width(), pixmap.height());
    let mut data = pixmap.take();
    for pixel in data.chunks_exact_mut(4) {
        let alpha = pixel[3] as u32;
        if alpha == 255 {
            continue;
        }
        if alpha == 0 {
            pixel[0] = 0;
            pixel[1] = 0;
            pixel[2] = 0;
        } else {
            for channel in &mut pixel[..3] {
                *channel = ((*channel as u32 * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
    let buffer = image::ImageBuffer::from_raw(width, height, data)?;
    Some(image::DynamicImage::ImageRgba8(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_decode_fails_soft() {
        // a text file is not a pdf: no panic, just None
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(decode_pdf_thumbnail(&path, 1, 256).is_none());
    }

    #[test]
    fn svg_decode_renders_true_colors() {
        use image::GenericImageView;
        // a red rectangle on the left half, transparent right half:
        // the tint-only svg element would discard the color, the
        // rasterizer must keep it, and the alpha must come back
        // straight (255 stays 255, zeros are zeros)
        let svg: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100"><rect width="100" height="100" fill="#ff0000"/></svg>"##;
        let image = decode_svg_bytes_dynamic(svg, 256).expect("a valid svg decodes");
        assert_eq!((image.width(), image.height()), (256, 128));
        assert_eq!(image.get_pixel(30, 30), image::Rgba([255, 0, 0, 255]));
        assert_eq!(image.get_pixel(200, 64), image::Rgba([0, 0, 0, 0]));
    }

    #[test]
    fn svg_decode_fails_soft() {
        // garbage bytes: no panic, just None
        assert!(decode_svg_bytes_dynamic(b"not an svg", 256).is_none());
    }

    #[test]
    fn titlebar_glyphs_rasterize() {
        use gpui::SvgRenderer;

        let assets = std::sync::Arc::new(Assets);
        let renderer = SvgRenderer::new(assets.clone());
        for (name, path) in [
            ("minimize", "icons/titlebar_minimize.svg"),
            ("maximize", "icons/titlebar_maximize.svg"),
            ("close", "icons/titlebar_close.svg"),
        ] {
            let bytes = assets
                .load(path)
                .unwrap()
                .unwrap_or_else(|| panic!("{name}: nothing at {path}"));
            let image = renderer
                .render_single_frame(bytes.as_ref(), 1.0)
                .unwrap_or_else(|err| panic!("{name}: did not parse: {err}"));
            let ink = image
                .as_bytes(0)
                .unwrap()
                .iter()
                .skip(3)
                .step_by(4)
                .filter(|&alpha| *alpha > 0)
                .count();
            assert!(ink > 20, "{name}: only {ink} inked pixels rasterized");
        }
    }

    #[test]
    fn thumbable_gates() {
        // videos are grid thumbs now (their poster frame, through
        // the bounded pool); playback itself stays with the default
        // handler
        assert!(is_video("clip.mp4"));
        assert!(is_video("CLIP.MKV"));
        assert!(is_video("clip.webm"));
        assert!(!is_video("clip.txt"));
        assert!(!is_video("song.mp3"));
        assert!(is_thumbable("clip.mp4"));
        assert!(is_thumbable("scan.pdf"));
        assert!(is_thumbable("SCAN.PDF"));
        assert!(!is_thumbable("notes.pdf.txt"));
        assert!(is_thumbable("photo.png"));
        assert!(!is_thumbable("report.docx"));
        // svgs rasterize in-process, no external tool involved
        assert!(is_svg("logo.svg"));
        assert!(is_svg("LOGO.SVG"));
        assert!(!is_svg("logo.svg.txt"));
        assert!(is_thumbable("logo.svg"));
    }
}
