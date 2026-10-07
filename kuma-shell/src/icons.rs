use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

pub struct KumaAssets;

impl AssetSource for KumaAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "icons/cpu.svg" => Some(include_bytes!("../icons/cpu.svg").as_slice()),
            "icons/volume.svg" => Some(include_bytes!("../icons/volume.svg").as_slice()),
            "icons/clock.svg" => Some(include_bytes!("../icons/clock.svg").as_slice()),
            "icons/gear.svg" => Some(include_bytes!("../icons/gear.svg").as_slice()),
            "icons/apps.svg" => Some(include_bytes!("../icons/apps.svg").as_slice()),
            "icons/media.svg" => Some(include_bytes!("../icons/media.svg").as_slice()),
            "icons/brightness.svg" => Some(include_bytes!("../icons/brightness.svg").as_slice()),
            "icons/sliders.svg" => Some(include_bytes!("../icons/sliders.svg").as_slice()),
            "icons/bell.svg" => Some(include_bytes!("../icons/bell.svg").as_slice()),
            "icons/power-profile.svg" => {
                Some(include_bytes!("../icons/power-profile.svg").as_slice())
            }
            "icons/dock.svg" => Some(include_bytes!("../icons/dock.svg").as_slice()),
            "icons/monitor.svg" => Some(include_bytes!("../icons/monitor.svg").as_slice()),
            "icons/bluetooth.svg" => Some(include_bytes!("../icons/bluetooth.svg").as_slice()),
            "icons/wifi.svg" => Some(include_bytes!("../icons/wifi.svg").as_slice()),
            "icons/bar.svg" => Some(include_bytes!("../icons/bar.svg").as_slice()),
            "icons/widgets.svg" => Some(include_bytes!("../icons/widgets.svg").as_slice()),
            "icons/image.svg" => Some(include_bytes!("../icons/image.svg").as_slice()),
            "icons/shield.svg" => Some(include_bytes!("../icons/shield.svg").as_slice()),
            "icons/moon.svg" => Some(include_bytes!("../icons/moon.svg").as_slice()),
            "icons/shield-lock.svg" => Some(include_bytes!("../icons/shield-lock.svg").as_slice()),
            "icons/shield-check.svg" => {
                Some(include_bytes!("../icons/shield-check.svg").as_slice())
            }
            "icons/shield-off.svg" => Some(include_bytes!("../icons/shield-off.svg").as_slice()),
            "icons/lock.svg" => Some(include_bytes!("../icons/lock.svg").as_slice()),
            "icons/key.svg" => Some(include_bytes!("../icons/key.svg").as_slice()),
            "icons/pencil.svg" => Some(include_bytes!("../icons/pencil.svg").as_slice()),
            "icons/refresh.svg" => Some(include_bytes!("../icons/refresh.svg").as_slice()),
            "icons/logout.svg" => Some(include_bytes!("../icons/logout.svg").as_slice()),
            "icons/power.svg" => Some(include_bytes!("../icons/power.svg").as_slice()),
            "icons/memory.svg" => Some(include_bytes!("../icons/memory.svg").as_slice()),
            "icons/temp.svg" => Some(include_bytes!("../icons/temp.svg").as_slice()),
            "icons/drive.svg" => Some(include_bytes!("../icons/drive.svg").as_slice()),
            "icons/check.svg" => Some(include_bytes!("../icons/check.svg").as_slice()),
            "icons/x.svg" => Some(include_bytes!("../icons/x.svg").as_slice()),
            "icons/chevron-right.svg" => {
                Some(include_bytes!("../icons/chevron-right.svg").as_slice())
            }
            "icons/arrow-left.svg" => Some(include_bytes!("../icons/arrow-left.svg").as_slice()),
            "icons/history.svg" => Some(include_bytes!("../icons/history.svg").as_slice()),
            "icons/link.svg" => Some(include_bytes!("../icons/link.svg").as_slice()),
            "icons/puzzle.svg" => Some(include_bytes!("../icons/puzzle.svg").as_slice()),
            "icons/order.svg" => Some(include_bytes!("../icons/order.svg").as_slice()),
            "icons/mic.svg" => Some(include_bytes!("../icons/mic.svg").as_slice()),
            "icons/trash.svg" => Some(include_bytes!("../icons/trash.svg").as_slice()),
            "icons/undo.svg" => Some(include_bytes!("../icons/undo.svg").as_slice()),
            "icons/copy.svg" => Some(include_bytes!("../icons/copy.svg").as_slice()),
            "icons/sun.svg" => Some(include_bytes!("../icons/sun.svg").as_slice()),
            "icons/cloud.svg" => Some(include_bytes!("../icons/cloud.svg").as_slice()),
            "icons/cloud-sun.svg" => Some(include_bytes!("../icons/cloud-sun.svg").as_slice()),
            "icons/cloud-fog.svg" => Some(include_bytes!("../icons/cloud-fog.svg").as_slice()),
            "icons/cloud-drizzle.svg" => {
                Some(include_bytes!("../icons/cloud-drizzle.svg").as_slice())
            }
            "icons/cloud-rain.svg" => Some(include_bytes!("../icons/cloud-rain.svg").as_slice()),
            "icons/cloud-snow.svg" => Some(include_bytes!("../icons/cloud-snow.svg").as_slice()),
            "icons/cloud-lightning.svg" => {
                Some(include_bytes!("../icons/cloud-lightning.svg").as_slice())
            }
            "icons/wifi-mid.svg" => Some(include_bytes!("../icons/wifi-mid.svg").as_slice()),
            "icons/wifi-low.svg" => Some(include_bytes!("../icons/wifi-low.svg").as_slice()),
            "icons/search.svg" => Some(include_bytes!("../icons/search.svg").as_slice()),
            "icons/arrow-down.svg" => Some(include_bytes!("../icons/arrow-down.svg").as_slice()),
            "icons/chevron-left.svg" => {
                Some(include_bytes!("../icons/chevron-left.svg").as_slice())
            }
            "icons/enter.svg" => Some(include_bytes!("../icons/enter.svg").as_slice()),
            "icons/terminal.svg" => Some(include_bytes!("../icons/terminal.svg").as_slice()),
            _ => None,
        };
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}
