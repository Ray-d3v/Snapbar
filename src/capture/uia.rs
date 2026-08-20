use std::collections::{HashMap, HashSet};

use anyhow::{Context as _, Result, anyhow};
use image::RgbaImage;
use uiautomation::{UIAutomation, UIElement, UITreeWalker};
use uiautomation::types::{Point, Rect as UiRect};
use xcap::Window;

use super::content_detector::{ContentCandidate, PixelRect};

mod scoring;

use self::scoring::{
    candidate_key, candidate_to_content_candidate, element_role, element_search_text,
    is_uia_candidate_rect,
};

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
        let screen_width = window
            .width()
            .context("Teamsウィンドウの幅を取得できませんでした")?;
        let screen_height = window
            .height()
            .context("Teamsウィンドウの高さを取得できませんでした")?;
        if screen_width == 0 || screen_height == 0 || image.width() == 0 || image.height() == 0 {
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
            image_width: image.width(),
            image_height: image.height(),
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

        let left = i64::from(rect.get_left()).maxhwindow_left);
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

pub(super) fn detect_content_candidates(geometry: WindowGeometry) -> Result<Vec<ContentCandidate>> {
    const SAMPLE_X: [f64; 7] = [0.06, 0.21, 0.36, 0.50, 0.64, 0.79, 0.94];
    const SAMPLE_Y: [f64; 7] = [0.06, 0.20, 0.35, 0.50, 0.65, 0.80, 0.94];
    const MAX_ANCESTORS: u8 = 10;

    let automation = UIAutomation::new()
        .or_else(|_| UIAutomation::new_direct())
        .context("Windows UI Automationを初期化できませんでした")?;
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
            collect_element_ancestors(
                element,
                &walker,
                geometry,
                MAX_ANCESTORS,
                &mut accumulators,
            );
        }
    }

    Ok(accumulators
        .values()
        .filter_map(|candidate| candidate_to_content_candidate(candidate, geometry))
        .collect())
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
                if is_uia_candidate_rect(rect, geometry)
                    && seen_for_point.insert(key)
                {
                    if let Some(existing) = accumulators.get_mut(&key) {
                        existing.hits = existing.hits.saturating_add(1);
                        existing.nearest_leaf_distance =
                            existing.nearest_leaf_distance.min(leaf_distance);
                    } else {
                        accumulators.insert(
                            key,
                            UiaAccumulator {
                                rect,
                                hits: 1,
                                nearest_leaf_distance: leaf_distance,
                                role: element_role(current.get_control_type().ok()),
                                text: element_search_text(&current),
                                is_content_element: current.is_content_element().unwrap_or(false),
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
    ((value * u64::from(target)) / u64::from(source))
        .min(u64::from(target)) as u32
}

fn scale_ceil(value: u64, target: u32, source: u32) -> u32 {
    (((value * u64::from(target)) + u64::from(source) - 1) / u64::from(source))
        .min(u64::from(target)) as u32
}

#[cfg(test)]
mod tests {
    use super::WindowGeometry;
    use crate::capture::content_detector::PixelRect;
    use uiautomation::types::Rect as UiRect;

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
}
