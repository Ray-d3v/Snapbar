use std::{
    borrow::Cow,
    ffi::c_void,
    fs,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

// Retained for diagnostics and regression tests. Automatic capture is UIA-authoritative.
#[allow(dead_code)]
mod content_detector;
mod engine;
mod flash;
mod local_share;
mod uia;

pub(crate) use self::local_share::detect_local_monitor_target;
pub use self::{
    engine::{CaptureEngine, CaptureSource},
    flash::{show_capture_flash, suspend_capture_flash},
    local_share::LocalMonitorCaptureTarget,
};
use anyhow::{Context as _, Result, anyhow};
use arboard::{Clipboard, ImageData};
use clipboard_win::{
    Clipboard as ClipboardGuard,
    raw::{get_vec, is_format_avail, register_format},
};
use image::{ColorType, ImageFormat};
use windows::Win32::{
    System::{Com::CoTaskMemFree, SystemInformation::GetLocalTime},
    UI::Shell::{FOLDERID_Screenshots, KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct CaptureReceipt {
    pub screen_rect: ScreenRect,
    pub target_window_id: Option<u32>,
    pub latency: Duration,
    pub frame_age: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureTarget {
    pub id: u32,
    pub title: String,
    pub app_name: String,
}

pub(super) fn copy_rgba_to_clipboard(width: u32, height: u32, bytes: &[u8]) -> Result<()> {
    let expected_len = width as usize * height as usize * 4;
    if bytes.len() != expected_len {
        return Err(anyhow!("共有画面フレームのバッファサイズが不正です"));
    }

    let mut last_error = None;
    for attempt in 0..3 {
        let result = Clipboard::new().and_then(|mut clipboard| {
            clipboard.set_image(ImageData {
                width: width as usize,
                height: height as usize,
                bytes: Cow::Borrowed(bytes),
            })
        });

        match result {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                if attempt < 2 {
                    thread::sleep(Duration::from_millis(30));
                }
            }
        }
    }

    Err(last_error
        .map(anyhow::Error::new)
        .unwrap_or_else(|| anyhow!("クリップボードへのコピーに失敗しました")))
    .context("クリップボードへのコピーに失敗しました")
}

pub fn save_clipboard_image_to_screenshots() -> Result<PathBuf> {
    let png = read_clipboard_png()?;
    let rgba = if png.is_none() {
        Some(
            Clipboard::new()
                .and_then(|mut clipboard| clipboard.get_image())
                .context("クリップボード画像を保存用に取得できませんでした")?,
        )
    } else {
        None
    };

    let folder = windows_screenshots_folder()?;
    fs::create_dir_all(&folder).with_context(|| {
        format!(
            "Windowsのスクリーンショット保存先を作成できませんでした: {}",
            folder.display()
        )
    })?;
    let path = next_screenshot_path(&folder);
    if let Some(png) = png {
        save_png_payload(&path, &png)?;
    } else if let Some(image) = rgba {
        let width = u32::try_from(image.width).context("画像の幅が大きすぎます")?;
        let height = u32::try_from(image.height).context("画像の高さが大きすぎます")?;
        let expected_len = image.width.saturating_mul(image.height).saturating_mul(4);
        if image.bytes.len() != expected_len {
            return Err(anyhow!("保存する画像のバッファサイズが不正です"));
        }
        save_rgba_buffer(&path, image.bytes.as_ref(), width, height).with_context(|| {
            format!(
                "スクリーンショットを保存できませんでした: {}",
                path.display()
            )
        })?;
    }
    Ok(path)
}

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

fn read_clipboard_png() -> Result<Option<Vec<u8>>> {
    let format = register_format("PNG").ok_or_else(|| anyhow!("PNG形式を登録できませんでした"))?;
    let mut last_error = None;
    for attempt in 0..5 {
        match ClipboardGuard::new() {
            Ok(_guard) => {
                if !is_format_avail(format.get()) {
                    return Ok(None);
                }
                let mut png = Vec::new();
                get_vec(format.get(), &mut png)
                    .context("クリップボードのPNGデータを取得できませんでした")?;
                return Ok(Some(png));
            }
            Err(error) => {
                last_error = Some(anyhow!(error));
                if attempt < 4 {
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("クリップボードを開けませんでした")))
}

fn save_png_payload(path: &Path, png: &[u8]) -> Result<()> {
    if png.is_empty() || !png.starts_with(PNG_SIGNATURE) {
        return Err(anyhow!("保存するPNGデータが空か不正です"));
    }
    fs::write(path, png).with_context(|| {
        format!(
            "スクリーンショットを保存できませんでした: {}",
            path.display()
        )
    })?;
    Ok(())
}

fn save_rgba_buffer(path: &Path, bytes: &[u8], width: u32, height: u32) -> image::ImageResult<()> {
    image::save_buffer_with_format(
        path,
        bytes,
        width,
        height,
        ColorType::Rgba8,
        ImageFormat::Png,
    )
}

pub(crate) fn windows_screenshots_folder() -> Result<PathBuf> {
    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_Screenshots, KF_FLAG_DONT_VERIFY, None)
            .context("Windowsのスクリーンショット保存先を取得できませんでした")?;
        let result = raw
            .to_string()
            .map(PathBuf::from)
            .context("スクリーンショット保存先のパスを読み取れませんでした");
        CoTaskMemFree(Some(raw.0.cast::<c_void>()));
        result
    }
}

fn next_screenshot_path(folder: &Path) -> PathBuf {
    let now = unsafe { GetLocalTime() };
    let stem = format!(
        "Screenshot {:04}-{:02}-{:02} {:02}{:02}{:02}",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond
    );
    let initial = folder.join(format!("{stem}.png"));
    if !initial.exists() {
        return initial;
    }

    for suffix in 2..=9999 {
        let candidate = folder.join(format!("{stem} ({suffix}).png"));
        if !candidate.exists() {
            return candidate;
        }
    }
    folder.join(format!("{stem} (duplicate).png"))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use image::{ColorType, ImageEncoder, codecs::png::PngEncoder};

    use super::{next_screenshot_path, save_png_payload, save_rgba_buffer};

    #[test]
    fn screenshot_path_uses_png_extension() {
        assert_eq!(
            next_screenshot_path(Path::new("C:/Screenshots"))
                .extension()
                .and_then(|value| value.to_str()),
            Some("png")
        );
    }

    #[test]
    fn png_payload_is_copied_exactly() {
        let path =
            std::env::temp_dir().join(format!("snapbar-png-payload-{}.png", std::process::id()));
        let pixels = [
            0_u8, 1, 2, 3, 255, 128, 64, 32, 17, 34, 51, 68, 240, 230, 220, 210,
        ];
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(&pixels, 2, 2, ColorType::Rgba8.into())
            .unwrap();
        save_png_payload(&path, &bytes).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rgba_fallback_roundtrips_losslessly() {
        let path =
            std::env::temp_dir().join(format!("snapbar-rgba-fallback-{}.png", std::process::id()));
        let bytes = [
            0_u8, 1, 2, 3, 255, 128, 64, 32, 17, 34, 51, 68, 240, 230, 220, 210,
        ];
        save_rgba_buffer(&path, &bytes, 2, 2).unwrap();
        let encoded = fs::read(&path).unwrap();
        let decoded = image::load_from_memory_with_format(&encoded, image::ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        assert_eq!(decoded.as_raw(), &bytes);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn corrupt_png_payload_is_rejected_without_writing() {
        let path =
            std::env::temp_dir().join(format!("snapbar-png-corrupt-{}.png", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut corrupt = super::PNG_SIGNATURE.to_vec();
        corrupt[0] ^= 1;
        corrupt.extend_from_slice(&[1, 2, 3]);
        assert!(save_png_payload(&path, &corrupt).is_err());
        assert!(!path.exists());
    }
}
