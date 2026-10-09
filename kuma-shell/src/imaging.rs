//! The image registry: every path into a `RenderImage` goes through here, so
//! the BGRA byte order gpui expects is handled in exactly one place, and tile
//! lifetime has an owner (ADR-0016). `resolve` hands out stable shared images
//! (one repaintable `image_id`, so one atlas tile per live asset), `release`
//! drops tiles when an asset leaves. The theme index and the LRU cache are
//! implementation details behind those two verbs.

use std::collections::{HashMap, VecDeque};
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

impl IconImage {
    /// A cheap copy that keeps the shared Arcs: stable image ids are the
    /// point of resolving through the registry (ADR-0016).
    pub fn clone_shared(&self) -> Self {
        match self {
            IconImage::Raster(image) => IconImage::Raster(image.clone()),
            IconImage::Svg(bytes) => IconImage::Svg(bytes.clone()),
        }
    }
}

/// Where themed icon files live: pixmaps + the icon theme dirs (system,
/// flatpak, user), the same roots the launcher searches.
pub fn icon_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    // user-installed icons first: the XDG spec gives the user data dir
    // precedence, and first root seen wins same-rank ties in the index
    if let Ok(data_home) = std::env::var("XDG_DATA_HOME") {
        roots.push(PathBuf::from(data_home).join("icons"));
    } else if let Ok(home) = std::env::var("HOME") {
        roots.push(PathBuf::from(&home).join(".local/share/icons"));
    }
    roots.push(PathBuf::from("/usr/share/pixmaps"));
    if let Ok(home) = std::env::var("HOME") {
        roots.push(PathBuf::from(&home).join(".local/share/flatpak/exports/share/icons"));
    }
    roots.push(PathBuf::from("/var/lib/flatpak/exports/share/icons"));
    roots.push(PathBuf::from("/usr/share/icons"));
    roots
}

/// Themed icon name → decoded icon, via the shared index.
/// Themed icon name or absolute path → decoded icon, downscaled to a
/// paint size. Size 0 entries are size-independent (SVG bytes).
fn decode_icon_sized(path: &Path, size: u32) -> Option<IconImage> {
    if path.extension().and_then(|ext| ext.to_str()) == Some("svg") {
        let bytes = std::fs::read(path).ok()?;
        return Some(IconImage::Svg(bytes.into()));
    }
    decode_thumbnail(path, size, size).map(Arc::new).map(IconImage::Raster)
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

fn open_image(path: &Path) -> Option<image::DynamicImage> {
    image::ImageReader::open(path).ok()?.decode().ok()
}

fn image_fit(path: &Path, max_width: u32, max_height: u32) -> Option<image::DynamicImage> {
    Some(open_image(path)?.thumbnail(max_width, max_height))
}

pub fn decode_file(path: &Path) -> Option<gpui::RenderImage> {
    Some(rgba_to_bgra_render_image(open_image(path)?.to_rgba8()))
}

/// Decode a wallpaper and squash it to exactly 112x112 RGB (37,632
/// bytes): the sample the palette generator works from. Aspect ratio
/// distorts, which is fine for color statistics.
pub fn decode_sampled(path: &Path) -> Option<Vec<u8>> {
    Some(
        open_image(path)?
            .resize_exact(112, 112, image::imageops::FilterType::Triangle)
            .to_rgb8()
            .into_raw(),
    )
}

pub fn decode_thumbnail(path: &Path, max_width: u32, max_height: u32) -> Option<gpui::RenderImage> {
    let mut thumb = image_fit(path, max_width, max_height)?.to_rgba8();
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

/// The image registry (ADR-0016): keyed by (name or path, paint size); slot
/// size 0 holds size-independent entries (SVG bytes). Bounded by LRU; a
/// missing icon caches as None so a repeating miss costs one map probe.
const REGISTRY_CAP: usize = 256;

#[derive(Clone, PartialEq, Eq, Hash)]
struct RegistryKey(String, u32);

#[derive(Default)]
struct Registry {
    map: HashMap<RegistryKey, Option<Arc<IconImage>>>,
    order: VecDeque<RegistryKey>,
}

static REGISTRY: std::sync::OnceLock<std::sync::Mutex<Registry>> = std::sync::OnceLock::new();
static INDEX: std::sync::OnceLock<HashMap<String, PathBuf>> = std::sync::OnceLock::new();

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY
        .get_or_init(|| std::sync::Mutex::new(Registry::default()))
        .lock()
        .expect("image registry poisoned")
}

impl Registry {
    fn get(&mut self, key: &RegistryKey) -> Option<Option<Arc<IconImage>>> {
        let hit = self.map.get(key).cloned();
        if hit.is_some() {
            // LRU bump: a used entry goes to the back of the eviction order
            self.order.retain(|k| k != key);
            self.order.push_back(key.clone());
        }
        hit
    }

    fn put(&mut self, key: RegistryKey, image: Option<Arc<IconImage>>) {
        self.order.retain(|k| *k != key);
        self.order.push_back(key.clone());
        self.map.insert(key, image);
        while self.map.len() > REGISTRY_CAP {
            let Some(oldest) = self.order.pop_front() else {
                self.map.clear();
                break;
            };
            // eviction sheds only the registry's own reference (ADR-0016):
            // a tile still painted from a caller's Arc lives until release
            self.map.remove(&oldest);
        }
    }
}

/// The theme index, walked once per process; everything that resolves icon
/// names shares it instead of walking the XDG roots again.
pub fn icon_index() -> &'static HashMap<String, PathBuf> {
    INDEX.get_or_init(|| build_icon_index(&icon_roots()))
}

/// Resolve an icon by themed name or absolute path, at a paint size (the
/// caller's element size, doubled for hidpi; rasters decode straight to that
/// size). The returned Arc is stable per (key, size): repainting it reuses
/// one atlas tile, so per-poll re-resolves are free. The first call for a
/// key decodes (sync; call from a background task when the key set is
/// large); misses cache as None.
pub fn resolve(key: &str, size: u32) -> Option<Arc<IconImage>> {
    if key.is_empty() {
        return None;
    }
    let mut registry = registry();
    let sized = RegistryKey(key.to_lowercase(), size);
    if let Some(hit) = registry.get(&sized) {
        return hit;
    }
    // SVG entries are size-independent: one slot serves every size
    let any_size = RegistryKey(key.to_lowercase(), 0);
    if let Some(hit) = registry.get(&any_size) {
        registry.put(sized, hit.clone());
        return hit;
    }
    let decoded = match key.starts_with('/') {
        true => decode_icon_sized(Path::new(key), size),
        false => icon_index()
            .get(&key.to_lowercase())
            .and_then(|path| decode_icon_sized(path, size)),
    };
    let size_independent = matches!(decoded, Some(IconImage::Svg(_)));
    let decoded = decoded.map(Arc::new);
    registry.put(
        if size_independent { any_size } else { sized },
        decoded.clone(),
    );
    decoded
}

/// Drop an image's atlas tiles: the explicit end of a caller-owned image
/// (the tray's replaced pixmap, the wallpaper's rotation predecessor,
/// Koguma's evicted thumbnails). The `img` element never drops the tile it
/// paints, so an asset leaving its collection must say so here.
pub fn release(image: &Arc<gpui::RenderImage>, cx: &mut gpui::App) {
    cx.drop_image(image.clone(), None);
}

/// The icon-typed form of `release` (notification history exits, tray items).
pub fn release_icon(icon: &Option<IconImage>, cx: &mut gpui::App) {
    if let Some(IconImage::Raster(image)) = icon {
        cx.drop_image(image.clone(), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_icon_root_precedes_system_roots() {
        // the shell dock and launcher resolve an icon by first root
        // seen: user-installed icons (e.g. Koguma's) must outrank the
        // system theme trees, per XDG data-dir precedence
        let roots = icon_roots();
        let first = roots.first().unwrap().to_string_lossy().into_owned();
        assert!(
            first.ends_with("/icons"),
            "user icons root should come first, got {first}"
        );
        assert!(!first.starts_with("/usr"));
    }

    // the workspace's own files stand in for installed icons: an svg and
    // a raster, both reachable as registry keys by absolute path
    fn svg_key() -> String {
        format!("{}/icons/clock.svg", env!("CARGO_MANIFEST_DIR"))
    }
    fn raster_key() -> String {
        format!("{}/../docs/kumaUI.png", env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn resolve_returns_a_stable_arc() {
        let a = resolve(&svg_key(), 32).expect("svg asset should decode");
        let b = resolve(&svg_key(), 48).expect("svg asset should decode");
        assert!(Arc::ptr_eq(&a, &b), "svg entries are size-independent");

        let a = resolve(&raster_key(), 32).expect("raster asset should decode");
        let b = resolve(&raster_key(), 32).expect("raster asset should decode");
        assert!(Arc::ptr_eq(&a, &b), "same key+size resolves to one Arc");
    }

    #[test]
    fn rasters_decode_to_the_requested_size() {
        // the source stands in for a theme png: big enough that the
        // downscale has to bite
        let original = open_image(Path::new(&raster_key())).expect("raster asset should open");
        assert!(original.width() > 48 && original.height() > 48);

        let fit = image_fit(Path::new(&raster_key()), 48, 48).expect("raster asset should fit");
        assert!(
            fit.width() <= 48 && fit.height() <= 48,
            "raster should downscale to the paint size at decode"
        );

        let icon = resolve(&raster_key(), 48).expect("raster asset should decode");
        assert!(matches!(&*icon, IconImage::Raster(_)));
    }

    #[test]
    fn misses_cache_as_none_and_stay_bounded() {
        for i in 0..(REGISTRY_CAP + 64) {
            assert!(resolve(&format!("/nonexistent-{i}.png"), 32).is_none());
        }
        let registry = registry();
        assert!(
            registry.map.len() <= REGISTRY_CAP,
            "the LRU cap bounds the cache even under negative caching"
        );
    }

    #[test]
    fn empty_keys_never_decode() {
        assert!(resolve("", 32).is_none());
    }
}
