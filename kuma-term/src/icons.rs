//! Embedded icons: the titlebar's window controls, the family's 16px
//! stroke style (the monitor's set is the origin, same bytes). gpui
//! renders them as alpha masks tinted by the element's text color, so
//! the SVGs' own colors are placeholders.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "icons/titlebar_minimize.svg" => {
                Some(include_bytes!("../icons/titlebar_minimize.svg"))
            }
            "icons/titlebar_maximize.svg" => {
                Some(include_bytes!("../icons/titlebar_maximize.svg"))
            }
            "icons/titlebar_close.svg" => Some(include_bytes!("../icons/titlebar_close.svg")),
            _ => None,
        };
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}
