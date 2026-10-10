//! The image registry: every path into a `RenderImage` goes through here, so
//! the BGRA byte order gpui expects is handled in exactly one place, and tile
//! lifetime has an owner (ADR-0016). `resolve` hands out stable shared images
//! (one repaintable `image_id`, so one atlas tile per live asset), `release`
//! drops tiles when an asset leaves. The theme index and the LRU cache are
//! implementation details behind those two verbs.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smallvec::SmallVec;

/// A decoded icon, either raster or vector; the launcher's app icons and
/// the tray's status icons share this. Named icons decode to Raster even
/// when the file is an SVG (gpui's `svg()` element is a single-color tint,
/// not a color render); direct producers like the nostr panel hand over
/// Svg bytes.
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

/// A full-color SVG rasterizer for named icons. gpui's `svg()` element paints
/// only an alpha mask tinted with the text color (a glyph tint), which
/// flattens multicolor artwork into a silhouette; app icons need resvg's
/// color render instead. Fonts load lazily, so building this is cheap.
fn svg_renderer() -> &'static gpui::SvgRenderer {
    static RENDERER: std::sync::OnceLock<gpui::SvgRenderer> = std::sync::OnceLock::new();
    RENDERER.get_or_init(|| gpui::SvgRenderer::new(std::sync::Arc::new(crate::icons::KumaAssets)))
}

/// Themed icon name or absolute path → decoded icon, downscaled to a
/// paint size. SVGs rasterize in full color at the same size rasters
/// decode at, so callers paint one shape of icon either way.
fn decode_icon_sized(path: &Path, size: u32) -> Option<IconImage> {
    if path.extension().and_then(|ext| ext.to_str()) == Some("svg") {
        let bytes = std::fs::read(path).ok()?;
        let parsed = svg_renderer().parse_svg(&bytes).ok()?;
        let image = svg_renderer()
            .render_parsed(
                &parsed,
                gpui::SvgSize::Size(gpui::Size::new(
                    gpui::DevicePixels(size as i32),
                    gpui::DevicePixels(size as i32),
                )),
            )
            .ok()?;
        return Some(IconImage::Raster(image));
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

/// One walk of the themed trees: the name→path index (preferring larger
/// sizes, then png over svg) plus a fingerprint of everything that can
/// change it. Each directory contributes its entry count and mtime, so
/// adding, removing, or renaming an icon file always moves the
/// fingerprint. File contents are not read: the index maps names to
/// paths, and decode freshness is the registry's business.
fn walk_icon_trees(roots: &[PathBuf]) -> (std::collections::HashMap<String, PathBuf>, u64) {
    let mut index: HashMap<String, (i32, PathBuf)> = HashMap::new();
    let mut dirs: Vec<(PathBuf, u64, Option<std::time::SystemTime>)> = Vec::new();
    let mut stack: Vec<PathBuf> = roots.to_vec();
    while let Some(dir) = stack.pop() {
        let mut count: u64 = 0;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.filter_map(|entry| entry.ok()) {
                count += 1;
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
        let mtime = std::fs::metadata(&dir).and_then(|meta| meta.modified()).ok();
        dirs.push((dir, count, mtime));
    }
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut fingerprinter = std::hash::DefaultHasher::new();
    for (path, count, mtime) in &dirs {
        path.hash(&mut fingerprinter);
        count.hash(&mut fingerprinter);
        mtime.hash(&mut fingerprinter);
    }
    let map = index
        .into_iter()
        .map(|(key, (_, path))| (key, path))
        .collect();
    (map, fingerprinter.finish())
}

/// Index of themed icon files (freedesktop icon dirs + pixmaps), preferring
/// larger sizes, then png over svg.
pub fn build_icon_index(roots: &[PathBuf]) -> std::collections::HashMap<String, PathBuf> {
    walk_icon_trees(roots).0
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

/// How often a themed-index miss may re-walk the trees. The tray polls
/// every 2s, so an item asking for a name that will never exist must not
/// walk per poll; a walk is only due after this long since the last one.
const INDEX_REFRESH_EVERY: Duration = Duration::from_secs(5);

/// The themed index plus its freshness state: the map, the fingerprint of
/// the walk it was built from, and the last refresh attempt.
#[derive(Default)]
struct ThemedIndex {
    map: HashMap<String, PathBuf>,
    fingerprint: Option<u64>,
    last_refresh: Option<Instant>,
}

impl ThemedIndex {
    /// The miss side of a named lookup: re-walk the trees if the rate
    /// limit allows and the fingerprint moved, adopt the new map, and
    /// report whether that happened (the caller purges the registry's
    /// stale negatives). Unchanged trees are a true miss: nothing to do.
    fn refresh_on_miss(&mut self, roots: &[PathBuf], now: Instant) -> bool {
        if self
            .last_refresh
            .is_some_and(|at| now.duration_since(at) < INDEX_REFRESH_EVERY)
        {
            return false;
        }
        self.last_refresh = Some(now);
        let (map, fingerprint) = walk_icon_trees(roots);
        if self.fingerprint == Some(fingerprint) {
            return false;
        }
        self.map = map;
        self.fingerprint = Some(fingerprint);
        true
    }
}

static INDEX: std::sync::OnceLock<std::sync::Mutex<ThemedIndex>> = std::sync::OnceLock::new();

fn themed_index() -> std::sync::MutexGuard<'static, ThemedIndex> {
    INDEX
        .get_or_init(|| std::sync::Mutex::new(ThemedIndex::default()))
        .lock()
        .expect("themed index poisoned")
}

/// Named-icon lookup through the themed index. A miss may mean the icon
/// landed after this process's first walk (an app installed mid-session):
/// re-walk, rate limited, and when the trees moved purge the registry's
/// cached misses before the re-look. The index and registry locks never
/// nest.
fn lookup_icon_path(key: &str) -> Option<PathBuf> {
    {
        let index = themed_index();
        if let Some(path) = index.map.get(key) {
            return Some(path.clone());
        }
    }
    let mut index = themed_index();
    if index.refresh_on_miss(&icon_roots(), Instant::now()) {
        drop(index);
        registry().purge_negatives();
        return themed_index().map.get(key).cloned();
    }
    index.map.get(key).cloned()
}

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

    /// Drop the cached misses: after a re-walk of the themed trees, a
    /// name may resolve where it did not before, and a decode that
    /// failed (a file caught mid-swap) may succeed now. Positive entries
    /// keep their Arcs, so painted tiles stay stable (ADR-0016).
    fn purge_negatives(&mut self) {
        self.order
            .retain(|key| matches!(self.map.get(key), Some(Some(_))));
        self.map.retain(|_, image| image.is_some());
    }
}

/// Resolve an icon by themed name or absolute path, at a paint size (the
/// caller's element size, doubled for hidpi; rasters decode straight to that
/// size). The returned Arc is stable per (key, size): repainting it reuses
/// one atlas tile, so per-poll re-resolves are free. The first call for a
/// key decodes (sync; call from a background task when the key set is
/// large); path misses cache as None. Named misses do not: a name that
/// resolves nothing today may resolve tomorrow (an app installed
/// mid-session), so those re-check the themed index.
pub fn resolve(key: &str, size: u32) -> Option<Arc<IconImage>> {
    if key.is_empty() {
        return None;
    }
    let mut cache = registry();
    let sized = RegistryKey(key.to_lowercase(), size);
    if let Some(hit) = cache.get(&sized) {
        return hit;
    }
    // SVG entries are size-independent: one slot serves every size
    let any_size = RegistryKey(key.to_lowercase(), 0);
    if let Some(hit) = cache.get(&any_size) {
        cache.put(sized, hit.clone());
        return hit;
    }
    // the named path may refresh the themed index, whose refresh purges
    // this registry's negatives: the two locks never nest
    drop(cache);
    let (decoded, named_miss) = match key.starts_with('/') {
        true => (decode_icon_sized(Path::new(key), size), false),
        false => match lookup_icon_path(&key.to_lowercase()) {
            Some(path) => (decode_icon_sized(&path, size), false),
            None => (None, true),
        },
    };
    let size_independent = matches!(decoded, Some(IconImage::Svg(_)));
    let decoded = decoded.map(Arc::new);
    if !named_miss {
        let mut registry = registry();
        registry.put(
            if size_independent { any_size } else { sized },
            decoded.clone(),
        );
    }
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
        // named svgs decode to full-color rasters (gpui's svg() element is
        // a tint, not a color render), keyed by size like raster files
        let a = resolve(&svg_key(), 32).expect("svg asset should decode");
        assert!(
            matches!(&*a, IconImage::Raster(_)),
            "named svg icons decode full-color, not as tint bytes"
        );
        let b = resolve(&svg_key(), 32).expect("svg asset should decode");
        assert!(Arc::ptr_eq(&a, &b), "same key+size resolves to one Arc");

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

    // a scratch tree for the walk: real dirs and files, real mtimes
    struct ScratchTree(PathBuf);

    impl ScratchTree {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("kuma-imaging-{}-{name}", std::process::id()));
            std::fs::create_dir_all(root.join("hicolor/scalable/apps")).expect("scratch dirs");
            Self(root)
        }

        fn apps_dir(&self) -> PathBuf {
            self.0.join("hicolor/scalable/apps")
        }

        fn icon(&self, stem: &str) -> PathBuf {
            self.apps_dir().join(format!("{stem}.svg"))
        }

        fn write_icon(&self, stem: &str) {
            std::fs::write(self.icon(stem), "<svg/>").expect("scratch icon");
        }
    }

    impl Drop for ScratchTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_walk_sees_new_files_and_moves_the_fingerprint() {
        let tree = ScratchTree::new("fingerprint");
        tree.write_icon("alpha");

        let (map, fingerprint) = walk_icon_trees(&[tree.0.clone()]);
        assert_eq!(map["alpha"], tree.icon("alpha"));
        assert!(
            !map.contains_key("beta"),
            "only what exists is indexed"
        );

        // a new file in a walked dir: same tree shape, new fingerprint
        tree.write_icon("beta");
        let (map, moved) = walk_icon_trees(&[tree.0.clone()]);
        assert_ne!(fingerprint, moved, "an added icon file moves the fingerprint");
        assert!(map.contains_key("alpha") && map.contains_key("beta"));
    }

    #[test]
    fn a_miss_refreshes_only_when_due_and_the_trees_moved() {
        let tree = ScratchTree::new("refresh");
        tree.write_icon("alpha");
        let roots = [tree.0.clone()];
        let mut index = ThemedIndex::default();
        let now = Instant::now();

        // first lookup builds from scratch and finds the name
        assert!(index.refresh_on_miss(&roots, now), "first miss builds");
        assert!(index.map.contains_key("alpha"));

        // within the rate limit: no re-walk even though "beta" is missing
        assert!(
            !index.refresh_on_miss(&roots, now + Duration::from_secs(1)),
            "the rate limit gates the re-walk"
        );
        assert!(!index.map.contains_key("beta"));

        // past the limit with unchanged trees: a walk happens but rebuilds nothing
        assert!(
            !index.refresh_on_miss(&roots, now + Duration::from_secs(6)),
            "unchanged trees are a true miss"
        );

        // past the limit with a new icon on disk: rebuild, name appears
        tree.write_icon("beta");
        assert!(
            index.refresh_on_miss(&roots, now + Duration::from_secs(12)),
            "moved trees rebuild the map"
        );
        assert!(index.map.contains_key("beta"));
    }

    #[test]
    fn purging_drops_only_cached_misses() {
        let mut registry = Registry::default();
        let hit = RegistryKey("hit".into(), 32);
        let miss = RegistryKey("miss".into(), 32);
        registry.put(
            hit.clone(),
            Some(Arc::new(IconImage::Raster(Arc::new(gpui::RenderImage::new(
                smallvec::smallvec![image::Frame::new(image::RgbaImage::new(1, 1))],
            ))))),
        );
        registry.put(miss, None);

        registry.purge_negatives();

        assert!(registry.map.contains_key(&hit), "positives survive");
        assert!(!registry.map.contains_key(&RegistryKey("miss".into(), 32)));
        assert!(
            registry.order.contains(&hit),
            "the eviction order stays in sync with the map"
        );
    }
}
