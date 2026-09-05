use std::{
    borrow::Cow,
    ffi::c_void,
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
mod output;
mod uia;

pub(crate) use self::local_share::detect_local_monitor_target;
pub use self::{
    engine::{CaptureEngine, CaptureSource},
    flash::{show_capture_flash, suspend_capture_flash},
    local_share::LocalMonitorCaptureTarget,
};
use anyhow::{Context as _, Result, anyhow};
use arboard::{Clipboard, ImageData};
pub use output::CaptureAuthorization;
pub(super) use output::output_capture;
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
    use std::path::Path;

    use super::next_screenshot_path;

    #[test]
    fn screenshot_path_uses_png_extension() {
        assert_eq!(
            next_screenshot_path(Path::new("C:/Screenshots"))
                .extension()
                .and_then(|value| value.to_str()),
            Some("png")
        );
    }
}
