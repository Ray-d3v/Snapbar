use std::{
    collections::{HashMap, HashSet},
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use image::RgbaImage;
use uiautomation::types::{ControlType, Handle, Point, Rect as UiRect, TreeScope};
use uiautomation::{UIAutomation, UIElement, UITreeWalker};
use xcap::Window;

use super::{
    ScreenRect,
    content_detector::{ContentCandidate, PixelRect},
};

mod scoring;

use self::scoring::{
    candidate_key, candidate_to_content_candidate, candidate_to_exclusion_candidate, element_role,
    element_search_text, is_uia_candidate_rect,
};

const PROVIDER_WARMUP_DELAY: Duration = Duration::from_millis(50);
const STABILITY_DELAY: Duration = Duration::from_millis(35);
const RECT_STABILITY_TOLERANCE: u32 = 3;

#[derive(Clone, Copy, Debug)]
pub(super) struct WindowGeometry {
    screen_left: i32,
    screen_top: i32,
    screen_width: u32,
    screen_height: u32,
    image_width: u32,
    image_height: u32,
}

impl WindowGeometry {
    pub(super) fn from_window(window: &Window, image: &RgbaImage) -> Result<Self> {
        let image_width = image.width();
        let image_height = image.height();
        let screen_width = window
            .width()
            .context("Teamsウィンドウの幅を取得できませんでした")?;
        let screen_height = window
            .height()
            .context("Teamsウィンドウの高さを取得できませんでした")?;
        if screen_width == 0 || screen_height == 0 || image_width == 0 || image_height == 0 {
            return Err(anyhow!("Teamsウィンドウのサイズが不正です"));
        }

        Ok(Self {
            screen_left: window
                .x()
                .context("TeamsウィンドウのX座標を取得できませんでした")?,
            screen_top: window
                .y()
                .context("TeamsウィンドウのY座標を取得できませんでした")?,
            screen_width,
            screen_height,
            image_width,
            image_height,
        })
    }

    pub(super) fn map_pixel_rect_to_screen(self, rect: PixelRect) -> Option<ScreenRect> {
        if rect.width == 0
            || rect.height == 0
            || rect.x.saturating_add(rect.width) > self.image_width
            || rect.y.saturating_add(rect.height) > self.image_height
        {
            return None;
        }

        let left = i64::from(self.screen_left)
            + i64::from(scale_floor(
                u64::from(rect.x),
                self.screen_width,
                self.image_width,
            ));
        let top = i64::from(self.screen_top)
            + i64::from(scale_floor(
                u64::from(rect.y),
                self.screen_height,
                self.image_height,
            ));
        let right = i64::from(self.screen_left)
            + i64::from(scale_ceil(
                u64::from(rect.x.saturating_add(rect.width)),
                self.screen_width,
                self.image_width,
            ));
        let bottom = i64::from(self.screen_top)
            + i64::from(scale_ceil(
                u64::from(rect.y.saturating_add(rect.height)),
                self.screen_height,
                self.image_height,
            ));

        if right <= left || bottom <= top {
            return None;
        }

        Some(ScreenRect {
            x: left.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            y: top.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            width: (right - left).min(i64::from(u32::MAX)) as u32,
            height: (bottom - top).min(i64::from(u32::MAX)) as u32,
        })
    }

    fn sample_point(self, x_fraction: f64, y_fraction: f64) -> Point {
        Point::new(
            add_fraction(self.screen_left, self.screen_width, x_fraction),
            add_fraction(self.screen_top, self.screen_height, y_fraction),
        )
    }

    fn map_ui_rect(self, rect: UiRect) -> Option<PixelRect> {
        let window_left = i64::from(self.screen_left);
        let window_top = i64::from(self.screen_top);
        let window_right = window_left + i64::from(self.screen_width);
        let window_bottom = window_top + i64::from(self.screen_height);

        let left = i64::from(rect.get_left()).max(window_left);
        let top = i64::from(rect.get_top()).max(window_top);
        let right = i64::from(rect.get_right()).min(window_right);
        let bottom = i64::from(rect.get_bottom()).min(window_bottom);
        if right <= left || bottom <= top {
            return None;
        }

        let local_left = (left - window_left) as u64;
        let local_top = (top - window_top) as u64;
        let local_right = (right - window_left) as u64;
        let local_bottom = (bottom - window_top) as u64;
        let pixel_left = scale_floor(local_left, self.image_width, self.screen_width);
        let pixel_top = scale_floor(local_top, self.image_height, self.screen_height);
        let pixel_right = scale_ceil(local_right, self.image_width, self.screen_width);
        let pixel_bottom = scale_ceil(local_bottom, self.image_height, self.screen_height);

        (pixel_right > pixel_left && pixel_bottom > pixel_top).then_some(PixelRect::new(
            pixel_left,
            pixel_top,
            pixel_right - pixel_left,
            pixel_bottom - pixel_top,
        ))
    }

    fn map_ui_rect_strict(self, rect: UiRect) -> Option<PixelRect> {
        let raw_left = i64::from(rect.get_left());
        let raw_top = i64::from(rect.get_top());
        let raw_right = i64::from(rect.get_right());
        let raw_bottom = i64::from(rect.get_bottom());
        if raw_right <= raw_left || raw_bottom <= raw_top {
            return None;
        }

        let window_left = i64::from(self.screen_left);
        let window_top = i64::from(self.screen_top);
        let window_right = window_left + i64::from(self.screen_width);
        let window_bottom = window_top + i64::from(self.screen_height);
        let intersection_left = raw_left.max(window_left);
        let intersection_top = raw_top.max(window_top);
        let intersection_right = raw_right.min(window_right);
        let intersection_bottom = raw_bottom.min(window_bottom);
        if intersection_right <= intersection_left || intersection_bottom <= intersection_top {
            return None;
        }

        let raw_area = u64::try_from(raw_right - raw_left).ok()?
            * u64::try_from(raw_bottom - raw_top).ok()?;
        let intersection_area = u64::try_from(intersection_right - intersection_left).ok()?
            * u64::try_from(intersection_bottom - intersection_top).ok()?;
        if intersection_area.saturating_mul(100) < raw_area.saturating_mul(98) {
            return None;
        }

        self.map_ui_rect(rect)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ElementRole {
    Image,
    Document,
    Custom,
    Pane,
    Group,
    Other,
    Text,
    Interactive,
    Chrome,
}

#[derive(Debug)]
struct UiaAccumulator {
    rect: PixelRect,
    hits: u32,
    nearest_leaf_distance: u8,
    role: ElementRole,
    text: String,
    is_content_element: bool,
}

type CandidateKey = (u32, u32, u32, u32);

#[derive(Debug, Default)]
pub(super) struct UiaDetection {
    pub authoritative_rect: Option<PixelRect>,
    pub fallback_candidates: Vec<ContentCandidate>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AuthoritativeCandidate {
    rect: PixelRect,
    rank: u8,
}

pub(super) fn detect_content_candidates(
    target_id: u32,
    geometry: WindowGeometry,
) -> Result<UiaDetection> {
    let automation = UIAutomation::new()
        .or_else(|_| UIAutomation::new_direct())
        .context("Windows UI Automationを初期化できませんでした")?;

    let authoritative_rect = detect_authoritative_rect(&automation, target_id, geometry)?;
    if authoritative_rect.is_some() {
        return Ok(UiaDetection {
            authoritative_rect,
            fallback_candidates: Vec::new(),
        });
    }

    Ok(UiaDetection {
        authoritative_rect: None,
        fallback_candidates: detect_sampled_candidates(&automation, geometry)?,
    })
}

fn detect_authoritative_rect(
    automation: &UIAutomation,
    target_id: u32,
    geometry: WindowGeometry,
) -> Result<Option<PixelRect>> {
    let mut first = scan_authoritative_rect(automation, target_id, geometry)?;
    if first.is_none() {
        let _ = automation.element_from_point(geometry.sample_point(0.50, 0.55));
        thread::sleep(PROVIDER_WARMUP_DELAY);
        first = scan_authoritative_rect(automation, target_id, geometry)?;
    }

    let Some(first) = first else {
        return Ok(None);
    };

    thread::sleep(STABILITY_DELAY);
    let Some(second) = scan_authoritative_rect(automation, target_id, geometry)? else {
        return Ok(None);
    };

    Ok(rects_are_stable(first, second, RECT_STABILITY_TOLERANCE).then_some(second))
}

fn scan_authoritative_rect(
    automation: &UIAutomation,
    target_id: u32,
    geometry: WindowGeometry,
) -> Result<Option<PixelRect>> {
    let root = automation
        .element_from_handle(Handle::from(target_id as isize))
        .context("TeamsウィンドウのUI Automationルートを取得できませんでした")?;
    let condition = automation
        .create_true_condition()
        .context("UI Automationの検索条件を作成できませんでした")?;
    let elements = root
        .find_all(TreeScope::Subtree, &condition)
        .context("TeamsのUI Automationツリーを走査できませんでした")?;

    let mut candidates = Vec::new();
    for element in elements {
        let Some(candidate) = authoritative_candidate_from_element(&element, geometry) else {
            continue;
        };
        insert_or_replace_candidate(&mut candidates, candidate);
    }

    select_unique_authoritative_candidate(&candidates).map(|candidate| candidate.rect)
}

fn authoritative_candidate_from_element(
    element: &UIElement,
    geometry: WindowGeometry,
) -> Option<AuthoritativeCandidate> {
    if element.is_offscreen().unwrap_or(true) {
        return None;
    }

    let name_rank = authoritative_name_rank(&element.get_name().ok()?)?;
    let control_rank = authoritative_control_rank(element.get_control_type().ok()?)?;
    let rect = geometry.map_ui_rect_strict(element.get_bounding_rectangle().ok()?)?;
    if !is_authoritative_content_rect(rect, geometry) {
        return None;
    }

    Some(AuthoritativeCandidate {
        rect,
        rank: name_rank.saturating_mul(10).saturating_add(control_rank),
    })
}

fn authoritative_name_rank(name: &str) -> Option<u8> {
    let normalized = normalize_accessible_name(name);
    if normalized.contains("共有") && normalized.contains("コンテンツ") {
        return Some(4);
    }
    if [
        "sharedcontent",
        "presentedcontent",
        "presentationcontent",
        "sharingcontent",
    ]
    .iter()
    .any(|phrase| normalized.contains(phrase))
    {
        return Some(4);
    }
    if normalized.contains("sharedscreen") || normalized.contains("screensharing") {
        return Some(3);
    }
    None
}

fn normalize_accessible_name(name: &str) -> String {
    name.chars()
        .filter(|character| {
            !character.is_whitespace()
                && !matches!(character, '_' | '-' | '–' | '—' | '・' | '/' | '\\')
        })
        .flat_map(|character| character.to_lowercase())
        .collect()
}

fn authoritative_control_rank(control_type: ControlType) -> Option<u8> {
    match control_type {
        ControlType::MenuItem => Some(5),
        ControlType::Document => Some(4),
        ControlType::Pane | ControlType::Custom | ControlType::Group | ControlType::Image => Some(3),
        _ => None,
    }
}

fn is_authoritative_content_rect(rect: PixelRect, geometry: WindowGeometry) -> bool {
    if rect.width < (geometry.image_width / 5).max(96)
        || rect.height < (geometry.image_height / 5).max(54)
    {
        return false;
    }

    let image_area = u64::from(geometry.image_width) * u64::from(geometry.image_height);
    let rect_area = u64::from(rect.width) * u64::from(rect.height);
    if image_area == 0 {
        return false;
    }
    let area_ratio = rect_area as f64 / image_area as f64;
    (0.08..=0.985).contains(&area_ratio)
}

fn insert_or_replace_candidate(
    candidates: &mut Vec<AuthoritativeCandidate>,
    candidate: AuthoritativeCandidate,
) {
    if let Some(existing) = candidates
        .iter_mut()
        .find(|existing| rects_are_stable(existing.rect, candidate.rect, RECT_STABILITY_TOLERANCE))
    {
        if candidate.rank > existing.rank {
            *existing = candidate;
        }
        return;
    }
    candidates.push(candidate);
}

fn select_unique_authoritative_candidate(
    candidates: &[AuthoritativeCandidate],
) -> Option<AuthoritativeCandidate> {
    let best_rank = candidates.iter().map(|candidate| candidate.rank).max()?;
    let mut best = candidates
        .iter()
        .copied()
        .filter(|candidate| candidate.rank == best_rank);
    let selected = best.next()?;
    best.next().is_none().then_some(selected)
}

fn rects_are_stable(left: PixelRect, right: PixelRect, tolerance: u32) -> bool {
    left.x.abs_diff(right.x) <= tolerance
        && left.y.abs_diff(right.y) <= tolerance
        && left.width.abs_diff(right.width) <= tolerance.saturating_mul(2)
        && left.height.abs_diff(right.height) <= tolerance.saturating_mul(2)
}

fn detect_sampled_candidates(
    automation: &UIAutomation,
    geometry: WindowGeometry,
) -> Result<Vec<ContentCandidate>> {
    const SAMPLE_X: [f64; 7] = [0.06, 0.21, 0.36, 0.50, 0.64, 0.79, 0.94];
    const SAMPLE_Y: [f64; 7] = [0.06, 0.20, 0.35, 0.50, 0.65, 0.80, 0.94];
    const MAX_ANCESTORS: u8 = 10;

    let walker = automation
        .get_raw_view_walker()
        .context("Windows UI Automationツリーを取得できませんでした")?;
    let mut accumulators = HashMap::<CandidateKey, UiaAccumulator>::new();

    for y_fraction in SAMPLE_Y {
        for x_fraction in SAMPLE_X {
            let point = geometry.sample_point(x_fraction, y_fraction);
            let Ok(element) = automation.element_from_point(point) else {
                continue;
            };
            collect_element_ancestors(element, &walker, geometry, MAX_ANCESTORS, &mut accumulators);
        }
    }

    let mut candidates = Vec::new();
    for candidate in accumulators.values() {
        if let Some(exclusion) = candidate_to_exclusion_candidate(candidate, geometry) {
            candidates.push(exclusion);
        }
        if let Some(content) = candidate_to_content_candidate(candidate, geometry) {
            candidates.push(content);
        }
    }
    Ok(candidates)
}

fn collect_element_ancestors(
    element: UIElement,
    walker: &UITreeWalker,
    geometry: WindowGeometry,
    max_ancestors: u8,
    accumulators: &mut HashMap<CandidateKey, UiaAccumulator>,
) {
    let mut current = element;
    let mut seen_for_point = HashSet::new();

    for leaf_distance in 0..=max_ancestors {
        if current.get_process_id().ok() == Some(std::process::id()) {
            break;
        }

        if let Ok(ui_rect) = current.get_bounding_rectangle() {
            if let Some(rect) = geometry.map_ui_rect(ui_rect) {
                let key = candidate_key(rect);
                if is_uia_candidate_rect(rect, geometry) && seen_for_point.insert(key) {
                    let role = element_role(current.get_control_type().ok());
                    let text = element_search_text(&current);
                    let is_content_element = current.is_content_element().unwrap_or(false);
                    if let Some(existing) = accumulators.get_mut(&key) {
                        existing.hits = existing.hits.saturating_add(1);
                        existing.nearest_leaf_distance =
                            existing.nearest_leaf_distance.min(leaf_distance);
                        existing.is_content_element |= is_content_element;
                        if !text.is_empty() && !existing.text.contains(&text) {
                            if !existing.text.is_empty() {
                                existing.text.push(' ');
                            }
                            existing.text.push_str(&text);
                        }
                    } else {
                        accumulators.insert(
                            key,
                            UiaAccumulator {
                                rect,
                                hits: 1,
                                nearest_leaf_distance: leaf_distance,
                                role,
                                text,
                                is_content_element,
                            },
                        );
                    }
                }
            }
        }

        let Ok(parent) = walker.get_parent(&current) else {
            break;
        };
        current = parent;
    }
}

fn add_fraction(origin: i32, size: u32, fraction: f64) -> i32 {
    let offset = ((f64::from(size.saturating_sub(1)) * fraction.clamp(0.0, 1.0)).round()) as i64;
    (i64::from(origin) + offset).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn scale_floor(value: u64, target: u32, source: u32) -> u32 {
    ((value * u64::from(target)) / u64::from(source)).min(u64::from(target)) as u32
}

fn scale_ceil(value: u64, target: u32, source: u32) -> u32 {
    (value * u64::from(target))
        .div_ceil(u64::from(source))
        .min(u64::from(target)) as u32
}

#[cfg(test)]
mod tests {
    use uiautomation::types::Rect as UiRect;

    use super::{
        AuthoritativeCandidate, WindowGeometry, authoritative_name_rank,
        select_unique_authoritative_candidate,
    };
    use crate::capture::{ScreenRect, content_detector::PixelRect};

    #[test]
    fn screen_coordinates_map_across_dpi_and_negative_monitor_origin() {
        let geometry = WindowGeometry {
            screen_left: -1920,
            screen_top: 120,
            screen_width: 1600,
            screen_height: 900,
            image_width: 2400,
            image_height: 1350,
        };
        let mapped = geometry
            .map_ui_rect(UiRect::new(-1760, 210, -560, 810))
            .expect("UIA rectangle should overlap the captured window");

        assert_eq!(mapped, PixelRect::new(240, 135, 1800, 900));
    }

    #[test]
    fn exact_measured_rect_maps_to_teams_relative_coordinates() {
        let geometry = WindowGeometry {
            screen_left: 828,
            screen_top: -1448,
            screen_width: 2255,
            screen_height: 1397,
            image_width: 2255,
            image_height: 1397,
        };
        let mapped = geometry
            .map_ui_rect_strict(UiRect::new(840, -1305, 3071, -51))
            .expect("measured shared content should map inside Teams");

        assert_eq!(mapped, PixelRect::new(12, 143, 2231, 1254));
    }

    #[test]
    fn pixel_rect_maps_back_to_negative_monitor_coordinates() {
        let geometry = WindowGeometry {
            screen_left: -1920,
            screen_top: 120,
            screen_width: 1600,
            screen_height: 900,
            image_width: 2400,
            image_height: 1350,
        };

        assert_eq!(
            geometry.map_pixel_rect_to_screen(PixelRect::new(240, 135, 1800, 900)),
            Some(ScreenRect {
                x: -1760,
                y: 210,
                width: 1200,
                height: 600,
            })
        );
    }

    #[test]
    fn authoritative_name_requires_strong_shared_content_semantics() {
        assert_eq!(authoritative_name_rank("共有コンテンツ"), Some(4));
        assert_eq!(authoritative_name_rank("共有  コンテンツ"), Some(4));
        assert_eq!(authoritative_name_rank("Shared content"), Some(4));
        assert_eq!(authoritative_name_rank("共有"), None);
        assert_eq!(authoritative_name_rank("コンテンツ"), None);
        assert_eq!(authoritative_name_rank("共有を停止"), None);
    }

    #[test]
    fn unique_highest_rank_candidate_is_selected() {
        let lower = AuthoritativeCandidate {
            rect: PixelRect::new(0, 0, 900, 600),
            rank: 43,
        };
        let menu_item = AuthoritativeCandidate {
            rect: PixelRect::new(12, 143, 2231, 1254),
            rank: 45,
        };

        assert_eq!(
            select_unique_authoritative_candidate(&[lower, menu_item]),
            Some(menu_item)
        );
    }

    #[test]
    fn ambiguous_equal_rank_candidates_fail_closed() {
        let left = AuthoritativeCandidate {
            rect: PixelRect::new(10, 100, 900, 600),
            rank: 45,
        };
        let right = AuthoritativeCandidate {
            rect: PixelRect::new(920, 100, 900, 600),
            rank: 45,
        };

        assert_eq!(select_unique_authoritative_candidate(&[left, right]), None);
    }
}
