use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext, Context, Entity, ObjectFit, Render, RenderImage, Window, div, img, prelude::*, rgb};

use crate::settings::{Settings, background_images, next_background};
use std::time::{Duration, Instant};

pub struct WallpaperView {
    settings: Entity<Settings>,
    /// The wallpaper to show, resolved once per settings change: a
    /// render must not stat the folder looking for it.
    wanted: PathBuf,
    image: Option<Arc<RenderImage>>,
    shown: Option<PathBuf>,
}

impl WallpaperView {
    pub fn new(settings: Entity<Settings>, cx: &mut Context<Self>) -> Self {
        let wanted = settings.read(cx).background.current_path();
        cx.observe(&settings, |this, _, cx| {
            this.wanted = this.settings.read(cx).background.current_path();
            cx.notify();
        })
        .detach();
        Self {
            settings,
            wanted,
            image: None,
            shown: None,
        }
    }
}

/// The rotation runner: when `rotate_minutes` is on, walk the folder's
/// images on that cadence by writing the next name through the
/// settings mutator; the wallpaper surface re-renders from the change,
/// and the config round-trip keeps the shown wallpaper across
/// restarts. A slow 30s tick reads the interval each time, so a
/// settings change applies without re-minting the loop; an interval
/// of 0 rests, and turning it on starts from the next tick.
pub fn run(settings: &Entity<Settings>, cx: &mut App) {
    let settings = settings.clone();
    cx.spawn(async move |cx| {
        // the interval's clock starts when rotation turns on, not when
        // the shell does: the first change waits a full interval
        let mut armed_since: Option<Instant> = None;
        loop {
            cx.background_executor().timer(Duration::from_secs(30)).await;
            let minutes = cx.update(|cx| settings.read(cx).background.rotate_minutes);
            let Some(minutes) = u64::from(minutes).checked_sub(1) else {
                // off: the clock disarms, the next on starts it fresh
                armed_since = None;
                continue;
            };
            let Some(since) = armed_since else {
                armed_since = Some(Instant::now());
                continue;
            };
            if since.elapsed() < Duration::from_secs((minutes + 1) * 60) {
                continue;
            }
            armed_since = Some(Instant::now());
            let next = cx.update(|cx| {
                settings.update(cx, |settings, _| {
                    let entries = background_images(&settings.background.folder);
                    next_background(&entries, &settings.background.current)
                })
            });
            if let Some(next) = next {
                cx.update(|cx| {
                    settings.update(cx, |settings, cx| settings.set_background(next, cx))
                });
            }
        }
    })
    .detach();
}

impl Render for WallpaperView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let wanted = self.wanted.clone();
        log::debug!(
            "wallpaper render: want={wanted:?} have_image={}",
            self.image.is_some()
        );
        if self.shown.as_ref() != Some(&wanted) {
            self.shown = Some(wanted.clone());
            // the old image keeps painting until the new decode lands (no
            // blank flash); the swap then releases its atlas tile (ADR-0016)
            let wanted_now = wanted.clone();
            cx.spawn(async move |this, cx| {
                let image = cx
                    .background_spawn(async move {
                        log::debug!("wallpaper decode start: {wanted:?}");
                        let image = crate::imaging::decode_file(&wanted).map(Arc::new);
                        log::debug!("wallpaper decoded: {}", image.is_some());
                        image
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    // the decode adopts only while its path is still
                    // the one wanted: a quick A→B→A must not let the
                    // slow A decode overwrite B's pixels (the render
                    // gate compares `shown` to `wanted`, so a stale
                    // adoption would stick until the next change)
                    if this.shown.as_ref() == Some(&wanted_now) {
                        if let Some(old) = std::mem::replace(&mut this.image, image) {
                            crate::imaging::release(&old, cx);
                        }
                    } else if let Some(image) = image {
                        // the decode lost the race: its pixels are
                        // released unused, not pinned in the atlas
                        crate::imaging::release(&image, cx);
                    }
                    cx.notify();
                });
            })
            .detach();
        }

        div()
            .size_full()
            .bg(rgb(0x11111B))
            .children(self.image.clone().map(|image| {
                img(gpui::ImageSource::Render(image))
                    .object_fit(ObjectFit::Cover)
                    .size_full()
            }))
    }
}
