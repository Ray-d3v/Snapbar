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
            "icons/lock-closed.svg" => Some(include_bytes!("../assets/icons/lock-closed.svg")),
            "icons/lock-open.svg" => Some(include_bytes!("../assets/icons/lock-open.svg")),
            "icons/more.svg" => Some(include_bytes!("../assets/icons/more.svg")),
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
            "lock-closed.svg",
            "lock-open.svg",
            "more.svg",
            "window.svg",
        ]
        .into_iter()
        .map(SharedString::from)
        .collect())
    }
}
