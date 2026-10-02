use std::path::PathBuf;
use std::sync::Arc;

use gpui::{Context, Entity, ObjectFit, Render, RenderImage, Window, div, img, prelude::*, rgb};

use crate::settings::Settings;

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

impl Render for WallpaperView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let wanted = self.settings.read(cx).background.current_path();
        log::info!(
            "wallpaper render: want={wanted:?} have_image={}",
            self.image.is_some()
        );
        if self.shown.as_ref() != Some(&wanted) {
            self.shown = Some(wanted.clone());
            self.image = None;
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
                    this.image = image;
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
