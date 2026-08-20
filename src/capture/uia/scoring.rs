use uiautomation::UIElement;
use uiautomation::types::ControlType;

use super::{CandidateKey, ElementRole, UiaAccumulator, WindowGeometry};
use crate::capture::content_detector::{ContentCandidate, PixelRect};

pub(super) fn candidate_to_content_candidate(
    candidate: &UiaAccumulator,
    geometry: WindowGeometry,
) -> Option<ContentCandidate> {
    let image_area = u64::from(geometry.image_width) * u64::from(geometry.image_height);
    if image_area == 0 {
        return None;
    }

    let area_ratio = candidate.rect_area() as f32 / image_area as f32;
    let width_ratio = candidate.rect.width as f32 / geometry.image_width as f32;
    let height_ratio = candidate.rect.height as f32 / geometry.image_height as f32;
    let center_x =
        (candidate.rect.x as f32 + candidate.rect.width as f32 / 2.0) / geometry.image_width as f32;
    let center_y = (candidate.rect.y as f32 + candidate.rect.height as f32 / 2.0)
        / geometry.image_height as f32;
    let normalized_dx = (center_x - 0.5).abs() * 2.0;
    let normalized_dy = (center_y - 0.5).abs() * 2.0;
    let centrality = 1.0
        - ((normalized_dx * normalized_dx + normalized_dy * normalized_dy).sqrt() / 2.0_f32.sqrt())
            .clamp(0.0, 1.0);
    let strong_hint = contains_any(&candidate.text, STRONG_CONTENT_HINTS);
    let weak_hint = contains_any(&candidate.text, WEAK_CONTENT_HINTS);
    let negative_hint = contains_any(&candidate.text, NEGATIVE_CONTENT_HINTS);

    let mut confidence = 0.05;
    confidence += if (0.10..=0.92).contains(&area_ratio) {
        0.14
    } else if (0.05..=0.98).contains(&area_ratio) {
        0.06
    } else {
        -0.15
    };
    confidence += 0.07 * (1.0 - ((area_ratio - 0.48).abs() / 0.48).min(1.0));
    confidence += centrality * 0.12;
    confidence += candidate.hits.min(18) as f32 / 18.0 * 0.20;
    confidence += 0.12 / (1.0 + candidate.nearest_leaf_distance as f32 * 0.55);
    confidence += match candidate.role {
        ElementRole::Image => 0.14,
        ElementRole::Document => 0.13,
        ElementRole::Custom => 0.10,
        ElementRole::Pane => 0.08,
        ElementRole::Group => 0.06,
        ElementRole::Other => 0.02,
        ElementRole::Text => -0.18,
        ElementRole::Interactive => -0.22,
        ElementRole::Chrome => -0.35,
    };
    if candidate.is_content_element {
        confidence += 0.05;
    }
    if width_ratio >= 0.35 {
        confidence += 0.04;
    }
    if height_ratio >= 0.35 {
        confidence += 0.04;
    }
    if strong_hint {
        confidence += 0.38;
    } else if weak_hint {
        confidence += 0.13;
    }
    if negative_hint {
        confidence -= 0.32;
    }

    let touching_edges = touching_edge_count(candidate.rect, geometry);
    confidence -= match touching_edges {
        4 | 3 => 0.28,
        2 => 0.12,
        1 => 0.03,
        _ => 0.0,
    };
    if area_ratio > 0.96 && !strong_hint {
        confidence -= 0.20;
    }

    Some(ContentCandidate::new(candidate.rect, confidence))
}

impl UiaAccumulator {
    fn rect_area(&self) -> u64 {
        u64::from(self.rect.width) * u64::from(self.rect.height)
    }
}

pub(super) fn element_role(control_type: Option<ControlType>) -> ElementRole {
    match control_type {
        Some(ControlType::Image) => ElementRole::Image,
        Some(ControlType::Document) => ElementRole::Document,
        Some(ControlType::Custom) => ElementRole::Custom,
        Some(ControlType::Pane) => ElementRole::Pane,
        Some(ControlType::Group) => ElementRole::Group,
        Some(ControlType::Text) => ElementRole::Text,
        Some(
            ControlType::Button
            | ControlType::CheckBox
            | ControlType::ComboBox
            | ControlType::Edit
            | ControlType::Hyperlink
            | ControlType::ListItem
            | ControlType::RadioButton
            | ControlType::Slider
            | ControlType::Spinner
            | ControlType::SplitButton
            | ControlType::TabItem
            | ControlType::TreeItem
            | ControlType::DataItem
            | ControlType::Thumb,
        ) => ElementRole::Interactive,
        Some(
            ControlType::ToolBar
            | ControlType::Menu
            | ControlType::MenuBar
            | ControlType::MenuItem
            | ControlType::StatusBar
            | ControlType::TitleBar
            | ControlType::Header
            | ControlType::HeaderItem
            | ControlType::ScrollBar
            | ControlType::Separator
            | ControlType::ToolTip
            | ControlType::AppBar,
        ) => ElementRole::Chrome,
        _ => ElementRole::Other,
    }
}

pub(super) fn element_search_text(element: &UIElement) -> String {
    [
        element.get_name().unwrap_or_default(),
        element.get_automation_id().unwrap_or_default(),
        element.get_classname().unwrap_or_default(),
        element.get_item_type().unwrap_or_default(),
        element.get_help_text().unwrap_or_default(),
    ]
    .join(" ")
    .to_lowercase()
}

pub(super) fn is_uia_candidate_rect(rect: PixelRect, geometry: WindowGeometry) -> bool {
    let min_width = (geometry.image_width / 10).max(80);
    let min_height = (geometry.image_height / 10).max(45);
    if rect.width < min_width || rect.height < min_height {
        return false;
    }
    let image_area = u64::from(geometry.image_width) * u64::from(geometry.image_height);
    let rect_area = u64::from(rect.width) * u64::from(rect.height);
    rect_area as f32 / image_area as f32 >= 0.04
}

fn touching_edge_count(rect: PixelRect, geometry: WindowGeometry) -> u8 {
    let tolerance_x = (geometry.image_width / 100).max(2);
    let tolerance_y = (geometry.image_height / 100).max(2);
    let mut count = 0;
    if rect.x <= tolerance_x {
        count += 1;
    }
    if rect.y <= tolerance_y {
        count += 1;
    }
    if rect
        .x
        .saturating_add(rect.width)
        .saturating_add(tolerance_x)
        >= geometry.image_width
    {
        count += 1;
    }
    if rect
        .y
        .saturating_add(rect.height)
        .saturating_add(tolerance_y)
        >= geometry.image_height
    {
        count += 1;
    }
    count
}

pub(super) fn candidate_key(rect: PixelRect) -> CandidateKey {
    (rect.x / 3, rect.y / 3, rect.width / 3, rect.height / 3)
}

fn contains_any(value: &str, hints: &[&str]) -> bool {
    hints.iter().any(|hint| value.contains(hint))
}

const STRONG_CONTENT_HINTS: &[&str] = &[
    "shared content",
    "shared screen",
    "screen share",
    "screen sharing",
    "presented content",
    "presentation content",
    "remote content",
    "remote screen",
    "powerpoint live",
    "content stage",
    "sharing content",
    "共有コンテンツ",
    "共有画面",
    "画面共有",
    "共有中の画面",
    "共有された画面",
    "発表者の画面",
    "ホワイトボード",
];

const WEAK_CONTENT_HINTS: &[&str] = &[
    "share",
    "present",
    "presentation",
    "content",
    "screen",
    "stage",
    "canvas",
    "remote",
    "slide",
    "共有",
    "画面",
    "発表",
    "プレゼン",
    "コンテンツ",
    "スライド",
];

const NEGATIVE_CONTENT_HINTS: &[&str] = &[
    "toolbar",
    "meeting controls",
    "control bar",
    "chat",
    "participants",
    "people",
    "reactions",
    "captions",
    "transcript",
    "navigation",
    "sidebar",
    "filmstrip",
    "gallery",
    "camera preview",
    "microphone",
    "leave button",
    "title bar",
    "ツールバー",
    "会議コントロール",
    "チャット",
    "参加者",
    "リアクション",
    "字幕",
    "トランスクリプト",
    "ナビゲーション",
    "サイドバー",
    "退出",
];

#[cfg(test)]
mod tests {
    use super::candidate_to_content_candidate;
    use crate::capture::{
        content_detector::PixelRect,
        uia::{ElementRole, UiaAccumulator, WindowGeometry},
    };

    #[test]
    fn shared_content_semantics_beat_generic_meeting_stage() {
        let geometry = WindowGeometry {
            screen_left: 0,
            screen_top: 0,
            screen_width: 1600,
            screen_height: 900,
            image_width: 1600,
            image_height: 900,
        };
        let shared = UiaAccumulator {
            rect: PixelRect::new(140, 90, 1180, 700),
            hits: 24,
            nearest_leaf_distance: 0,
            role: ElementRole::Custom,
            text: "shared content canvas".to_string(),
            is_content_element: true,
        };
        let stage = UiaAccumulator {
            rect: PixelRect::new(0, 40, 1600, 820),
            hits: 40,
            nearest_leaf_distance: 3,
            role: ElementRole::Pane,
            text: "meeting stage".to_string(),
            is_content_element: true,
        };

        let shared_confidence = candidate_to_content_candidate(&shared, geometry)
            .expect("shared candidate")
            .confidence;
        let stage_confidence = candidate_to_content_candidate(&stage, geometry)
            .expect("stage candidate")
            .confidence;

        assert!(shared_confidence > stage_confidence);
        assert!(shared_confidence >= 0.8);
    }
}
