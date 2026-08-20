use std::{borrow::Cow, thread, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use arboard::{Clipboard, ImageData};
use image::{RgbaImage, imageops};
use xcap::Window;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CropMode {
    FullWindow,
    TeamsContentPreset,
}

impl CropMode {
    pub fn toggled(self) -> Self {
        match self {
            Self::FullWindow => Self::TeamsContentPreset,
            Self::TeamsContentPreset => Self::FullWindow,
        }
    }
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

pub fn capture_to_clipboard(target_id: u32, crop_mode: CropMode) -> Result<()> {
    let target = Window::all()
        .context("ウィンドウ一覧を再取得できませんでした")?
        .into_iter()
        .find(|window| window.id().ok() == Some(target_id))
        .ok_or_else(|| anyhow!("選択中のTeamsウィンドウが見つかりません"))?;

    if target.is_minimized().unwrap_or(false) {
        return Err(anyhow!("選択中のTeamsウィンドウが最小化されています"));
    }

    let image = target
        .capture_image()
        .context("Teamsウィンドウのキャプチャに失敗しました")?;
    let image = apply_crop(image, crop_mode);
    copy_to_clipboard(image)
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

fn apply_crop(image: RgbaImage, mode: CropMode) -> RgbaImage {
    if mode == CropMode::FullWindow {
        return image;
    }

    let width = image.width();
    let height = image.height();
    if width < 640 || height < 360 {
        return image;
    }

    let side = ((width as f32 * 0.004).round() as u32).min(8);
    let top = ((height as f32 * 0.035).round() as u32).max(24).min(48);
    let bottom = ((height as f32 * 0.018).round() as u32).max(8).min(28);

    let cropped_width = width.saturating_sub(side.saturating_mul(2));
    let cropped_height = height.saturating_sub(top.saturating_add(bottom));
    if cropped_width < 320 || cropped_height < 180 {
        return image;
    }

    imageops::crop_imm(&image, side, top, cropped_width, cropped_height).to_image()
}

fn copy_to_clipboard(image: RgbaImage) -> Result<()> {
    let width = image.width() as usize;
    let height = image.height() as usize;
    let bytes = image.into_raw();
    let mut last_error = None;

    for attempt in 0..3 {
        let result = Clipboard::new().and_then(|mut clipboard| {
            clipboard.set_image(ImageData {
                width,
                height,
                bytes: Cow::Borrowed(&bytes),
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

#[cfg(test)]
mod tests {
    use super::{WindowDescriptor, score_window};

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
}
