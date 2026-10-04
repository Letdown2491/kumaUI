//! Image decoding for gpui: every path into a `RenderImage` goes through here,
//! so the BGRA byte order gpui expects is handled in exactly one place.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use smallvec::SmallVec;

/// A decoded icon, either raster or vector; the launcher's app icons and
/// the tray's status icons share this.
#[derive(Clone, Debug)]
pub enum IconImage {
    Raster(Arc<gpui::RenderImage>),
    Svg(Arc<[u8]>),
}

/// Where themed icon files live: pixmaps + the icon theme dirs (system,
/// flatpak, user), the same roots the launcher searches.
pub fn icon_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/usr/share/pixmaps")];
    if let Ok(home) = std::env::var("HOME") {
        roots.push(PathBuf::from(&home).join(".local/share/flatpak/exports/share/icons"));
    }
    roots.push(PathBuf::from("/var/lib/flatpak/exports/share/icons"));
    roots.push(PathBuf::from("/usr/share/icons"));
    roots
}

/// Themed icon name → decoded icon, via the shared index.
pub fn decode_icon_file(path: &Path) -> Option<IconImage> {
    if path.extension().and_then(|ext| ext.to_str()) == Some("svg") {
        let bytes = std::fs::read(path).ok()?;
        return Some(IconImage::Svg(bytes.into()));
    }
    decode_file(path).map(Arc::new).map(IconImage::Raster)
}

/// SNI `IconPixmap`: ARGB32 in network byte order, width, height, straight
/// to the BGRA `RenderImage` gpui wants.
pub fn argb_to_render_image(
    width: i32,
    height: i32,
    mut data: Vec<u8>,
) -> Option<gpui::RenderImage> {
    let width = usize::try_from(width).ok()?;
    let height = usize::try_from(height).ok()?;
    if data.len() != width * height * 4 {
        return None;
    }
    // wire order is A,R,G,B per pixel; gpui wants B,G,R,A
    for pixel in data.chunks_exact_mut(4) {
        pixel.swap(0, 3);
        pixel.swap(1, 2);
    }
    let bgra = image::RgbaImage::from_raw(width as u32, height as u32, data)?;
    Some(gpui::RenderImage::new(SmallVec::from_buf([
        image::Frame::new(bgra),
    ])))
}

pub fn decode_rgba(bytes: &[u8]) -> Option<gpui::RenderImage> {
    let image = image::load_from_memory(bytes).ok()?;
    Some(rgba_to_bgra_render_image(image.to_rgba8()))
}

pub fn decode_file(path: &Path) -> Option<gpui::RenderImage> {
    let image = image::ImageReader::open(path).ok()?.decode().ok()?;
    Some(rgba_to_bgra_render_image(image.to_rgba8()))
}

/// Decode a wallpaper and squash it to exactly 112x112 RGB (37,632
/// bytes): the sample the palette generator works from. Aspect ratio
/// distorts, which is fine for color statistics.
pub fn decode_sampled(path: &Path) -> Option<Vec<u8>> {
    let image = image::ImageReader::open(path).ok()?.decode().ok()?;
    Some(
        image
            .resize_exact(112, 112, image::imageops::FilterType::Triangle)
            .to_rgb8()
            .into_raw(),
    )
}

pub fn decode_thumbnail(path: &Path, max_width: u32, max_height: u32) -> Option<gpui::RenderImage> {
    let image = image::ImageReader::open(path).ok()?.decode().ok()?;
    let mut thumb = image.thumbnail(max_width, max_height).to_rgba8();
    for pixel in thumb.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Some(gpui::RenderImage::new(SmallVec::from_buf([
        image::Frame::new(thumb),
    ])))
}

fn rgba_to_bgra_render_image(rgba: image::RgbaImage) -> gpui::RenderImage {
    let mut bgra = rgba;
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    gpui::RenderImage::new(SmallVec::from_buf([image::Frame::new(bgra)]))
}

/// Index of themed icon files (freedesktop icon dirs + pixmaps), preferring
/// larger sizes, then png over svg.
pub fn build_icon_index(roots: &[PathBuf]) -> std::collections::HashMap<String, PathBuf> {
    let mut index = std::collections::HashMap::new();
    let mut stack: Vec<PathBuf> = roots.to_vec();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let is_icon = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext == "png" || ext == "svg");
            if !is_icon {
                continue;
            }
            let key = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let rank = icon_rank(&path);
            match index.get(&key) {
                Some((existing_rank, _)) if *existing_rank >= rank => {}
                _ => {
                    index.insert(key, (rank, path));
                }
            }
        }
    }
    index
        .into_iter()
        .map(|(key, (_, path))| (key, path))
        .collect()
}

/// Larger theme sizes rank higher; "scalable" beats everything; png beats svg.
pub fn icon_rank(path: &Path) -> i32 {
    let mut rank = 0;
    if path.extension().and_then(|ext| ext.to_str()) == Some("png") {
        rank += 1;
    }
    if let Some(size_dir) = path.parent().and_then(|dir| dir.file_name())
        && let Some(name) = size_dir.to_str()
    {
        if name == "scalable" {
            rank += 10_000;
        } else if let Ok(size) = name.split(['x', 'X']).next().unwrap_or("0").parse::<i32>() {
            rank += size;
        }
    }
    rank
}

/// Process-wide icon cache: the theme index is walked once, decoded icons
/// are kept by name; dock recreations (and any future icon-hungry
/// surface) hit memory instead of re-walking the filesystem.

/// Themed icon name → decoded icon, through the cache. The first call for
/// an uncached name builds the index (slow, once); everything after is a
/// map lookup.
pub fn cached_icon(icon_name: &str) -> Option<IconImage> {
    let key = icon_name.to_lowercase();
    if key.is_empty() {
        return None;
    }
    static INDEX: std::sync::OnceLock<HashMap<String, PathBuf>> = std::sync::OnceLock::new();
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Option<IconImage>>>> =
        std::sync::OnceLock::new();
    let index = INDEX.get_or_init(|| build_icon_index(&icon_roots()));
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().expect("icon cache poisoned").get(&key) {
        return hit.clone();
    }
    let decoded = index.get(&key).and_then(|path| decode_icon_file(path));
    cache
        .lock()
        .expect("icon cache poisoned")
        .insert(key, decoded.clone());
    decoded
}

/// An icon spec from the wild: a filesystem path decodes directly, a themed
/// name resolves through the cache.
pub fn resolve_icon(spec: &str) -> Option<IconImage> {
    if spec.is_empty() {
        return None;
    }
    if spec.starts_with('/') {
        return decode_icon_file(Path::new(spec));
    }
    cached_icon(spec)
}
