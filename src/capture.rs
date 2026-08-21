use std::{
    borrow::Cow,
    ffi::c_void,
    fs,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

mod content_detector;
mod engine;
mod flash;
mod uia;

pub use self::{engine::CaptureEngine, flash::show_capture_flash};
use anyhow::{Context as _, Result, anyhow};
use arboard::{Clipboard, ImageData};
use image::{ColorType, ImageFormat};
use windows::Win32::{
    Foundation::SYSTEMTIME,
    System::{Com::CoTaskMemFree, SystemInformation::GetLocalTime},
    UI::Shell::{FOLDERID_Screenshots, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
};
use xcap::Window;

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
    pub latency: Duration,
    pub frame_age: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureTarget {
    pub id: u32,
    pub title: String,
    pub app_name: String,
}

#[derive(Debug)]
struct Candidate {
    target: CaptureTarget,
    score: i32,
}

#[derive(Clone, Copy, Debug)]
struct WindowDescriptor<'a> {
    app_name: &'a str,
    title: &'a str,
    width: u32,
    height: u32,
    focused: bool,
    z_index: usize,
}

pub fn discover_teams_targets() -> Result<Vec<CaptureTarget>> {
    let windows = Window::all().context("ウィンドウ一覧を取得できませんでした")?;
    let mut candidates = Vec::new();

    for (z_index, window) in windows.into_iter().enumerate() {
        if window.is_minimized().unwrap_or(false) {
            continue;
        }

        let id = match window.id() {
            Ok(id) => id,
            Err(_) => continue,
        };
        let app_name = window.app_name().unwrap_or_default();
        let title = window.title().unwrap_or_default();
        let width = window.width().unwrap_or_default();
        let height = window.height().unwrap_or_default();
        let focused = window.is_focused().unwrap_or(false);

        let score = score_window(&WindowDescriptor {
            app_name: &app_name,
            title: &title,
            width,
            height,
            focused,
            z_index,
        });
        if score <= 0 {
            continue;
        }

        candidates.push(Candidate {
            target: CaptureTarget {
                id,
                title,
                app_name,
            },
            score,
        });
    }

    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.target.title.cmp(&right.target.title))
    });
    candidates.dedup_by_key(|candidate| candidate.target.id);

    Ok(candidates
        .into_iter()
        .map(|candidate| candidate.target)
        .collect())
}

fn score_window(window: &WindowDescriptor<'_>) -> i32 {
    let app = window.app_name.to_lowercase();
    let title = window.title.to_lowercase();
    let app_is_teams = app.contains("teams") || app.contains("ms-teams");
    let title_is_teams = title == "microsoft teams"
        || title.ends_with(" | microsoft teams")
        || title.ends_with(" - microsoft teams")
        || title.starts_with("microsoft teams |")
        || title.starts_with("microsoft teams -");

    if !app_is_teams && !title_is_teams {
        return 0;
    }

    let mut score = 0;
    if app_is_teams {
        score += 100;
    }
    if title_is_teams {
        score += 35;
    }

    const SHARE_HINTS: [&str; 11] = [
        "shared",
        "sharing",
        "screen",
        "present",
        "presentation",
        "meeting",
        "共有",
        "画面",
        "発表",
        "プレゼン",
        "会議",
    ];
    for hint in SHARE_HINTS {
        if title.contains(hint) {
            score += 18;
        }
    }

    if window.width >= 640 && window.height >= 360 {
        score += 10;
    }
    if window.height > 0 && window.width as f32 / window.height as f32 >= 1.2 {
        score += 8;
    }
    if window.focused {
        score += 20;
    }

    score + 20_i32.saturating_sub(window.z_index.min(20) as i32)
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
    let image = Clipboard::new()
        .and_then(|mut clipboard| clipboard.get_image())
        .context("クリップボード画像を保存用に取得できませんでした")?;
    let width = u32::try_from(image.width).context("画像の幅が大きすぎます")?;
    let height = u32::try_from(image.height).context("画像の高さが大きすぎます")?;
    let expected_len = image.width.saturating_mul(image.height).saturating_mul(4);
    if image.bytes.len() != expected_len {
        return Err(anyhow!("保存する画像のバッファサイズが不正です"));
    }

    let folder = windows_screenshots_folder()?;
    fs::create_dir_all(&folder).with_context(|| {
        format!(
            "Windowsのスクリーンショット保存先を作成できませんでした: {}",
            folder.display()
        )
    })?;
    let path = next_screenshot_path(&folder);
    image::save_buffer_with_format(
        &path,
        image.bytes.as_ref(),
        width,
        height,
        ColorType::Rgba8,
        ImageFormat::Png,
    )
    .with_context(|| {
        format!(
            "スクリーンショットを保存できませんでした: {}",
            path.display()
        )
    })?;
    Ok(path)
}

fn windows_screenshots_folder() -> Result<PathBuf> {
    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_Screenshots, KF_FLAG_DEFAULT, None)
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
    let mut now = SYSTEMTIME::default();
    unsafe {
        GetLocalTime(&mut now);
    }
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

    use super::{WindowDescriptor, next_screenshot_path, score_window};

    #[test]
    fn teams_shared_window_scores_higher_than_main_window() {
        let main = WindowDescriptor {
            app_name: "ms-teams.exe",
            title: "Microsoft Teams",
            width: 1280,
            height: 800,
            focused: false,
            z_index: 2,
        };
        let shared = WindowDescriptor {
            app_name: "ms-teams.exe",
            title: "Shared screen - Project meeting | Microsoft Teams",
            width: 1600,
            height: 900,
            focused: false,
            z_index: 1,
        };

        assert!(score_window(&shared) > score_window(&main));
    }

    #[test]
    fn non_teams_window_is_rejected() {
        let browser = WindowDescriptor {
            app_name: "msedge.exe",
            title: "Teams documentation",
            width: 1280,
            height: 800,
            focused: true,
            z_index: 0,
        };

        assert_eq!(score_window(&browser), 0);
    }

    #[test]
    fn topmost_candidate_gets_a_small_bonus() {
        let front = WindowDescriptor {
            app_name: "ms-teams.exe",
            title: "Microsoft Teams meeting",
            width: 1280,
            height: 720,
            focused: false,
            z_index: 0,
        };
        let back = WindowDescriptor {
            z_index: 20,
            ..front
        };

        assert!(score_window(&front) > score_window(&back));
    }

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
