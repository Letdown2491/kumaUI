use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext, Context, Entity, ObjectFit, Render, RenderImage, Window, div, img, prelude::*, rgb};

use crate::settings::{Settings, background_images, next_background};
use std::time::{Duration, Instant};

pub struct WallpaperView {
    settings: Entity<Settings>,
    image: Option<Arc<RenderImage>>,
    shown: Option<PathBuf>,
}

impl WallpaperView {
    pub fn new(settings: Entity<Settings>, cx: &mut Context<Self>) -> Self {
        cx.observe(&settings, |_, _, cx| cx.notify()).detach();
        Self {
            settings,
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
        let wanted = self.settings.read(cx).background.current_path();
        log::info!(
            "wallpaper render: want={wanted:?} have_image={}",
            self.image.is_some()
        );
        if self.shown.as_ref() != Some(&wanted) {
            self.shown = Some(wanted.clone());
            // the old image keeps painting until the new decode lands (no
            // blank flash); the swap then releases its atlas tile (ADR-0016)
            cx.spawn(async move |this, cx| {
                let image = cx
                    .background_spawn(async move {
                        log::info!("wallpaper decode start: {wanted:?}");
                        let image = crate::imaging::decode_file(&wanted).map(Arc::new);
                        log::info!("wallpaper decoded: {}", image.is_some());
                        image
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(old) = std::mem::replace(&mut this.image, image) {
                        crate::imaging::release(&old, cx);
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
