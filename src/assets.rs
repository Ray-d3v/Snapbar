use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "icons/alert.svg" => Some(include_bytes!("../assets/icons/alert.svg")),
            "icons/camera.svg" => Some(include_bytes!("../assets/icons/camera.svg")),
            "icons/check.svg" => Some(include_bytes!("../assets/icons/check.svg")),
            "icons/crop.svg" => Some(include_bytes!("../assets/icons/crop.svg")),
            "icons/folder.svg" => Some(include_bytes!("../assets/icons/folder.svg")),
            "icons/lock-closed.svg" => Some(include_bytes!("../assets/icons/lock-closed.svg")),
            "icons/lock-open.svg" => Some(include_bytes!("../assets/icons/lock-open.svg")),
            "icons/menu.svg" => Some(include_bytes!("../assets/icons/menu.svg")),
            "icons/more.svg" => Some(include_bytes!("../assets/icons/more.svg")),
            "icons/power.svg" => Some(include_bytes!("../assets/icons/power.svg")),
            "icons/refresh.svg" => Some(include_bytes!("../assets/icons/refresh.svg")),
            "icons/window.svg" => Some(include_bytes!("../assets/icons/window.svg")),
            _ => None,
        };

        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        if path != "icons" {
            return Ok(Vec::new());
        }

        Ok([
            "alert.svg",
            "camera.svg",
            "check.svg",
            "crop.svg",
            "folder.svg",
            "lock-closed.svg",
            "lock-open.svg",
            "menu.svg",
            "more.svg",
            "power.svg",
            "refresh.svg",
            "window.svg",
        ]
        .into_iter()
        .map(SharedString::from)
        .collect())
    }
}
