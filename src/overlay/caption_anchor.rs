//! Teams may draw a shorter custom caption than DWM's native button rectangle.
//! Prefer a consistent, semantically identified UIA caption-button row. No pixel
//! or monitor-resolution heuristics determine the vertical attachment.
use super::{RectI, extended_frame_bounds, get_window_rect};
use std::time::{Duration, Instant};
use uiautomation::{
    UIAutomation,
    types::{ControlType, ElementMode, Handle, TreeScope, UIProperty},
};
use windows::Win32::{Foundation::HWND, UI::HiDpi::GetDpiForWindow};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CaptionObservation {
    target: isize,
    window_width: i32,
    window_height: i32,
    frame_width: i32,
    frame_height: i32,
    frame_inset_x: i32,
    frame_inset_y: i32,
    dpi: u32,
    bottom_from_frame: i32,
}

impl CaptionObservation {
    fn new(hwnd: HWND, window: RectI, frame: RectI, dpi: u32, bottom: i32) -> Self {
        Self {
            target: hwnd.0 as isize,
            window_width: window.width(),
            window_height: window.height(),
            frame_width: frame.width(),
            frame_height: frame.height(),
            frame_inset_x: frame.left - window.left,
            frame_inset_y: frame.top - window.top,
            dpi,
            bottom_from_frame: bottom - frame.top,
        }
    }

    pub(super) fn bottom(self, hwnd: HWND, window: RectI, frame: RectI) -> Option<i32> {
        let current = Self::new(
            hwnd,
            window,
            frame,
            unsafe { GetDpiForWindow(hwnd) },
            frame.top + self.bottom_from_frame,
        );
        (current == self).then_some(frame.top + self.bottom_from_frame)
    }
}

pub(super) struct CaptionProbe {
    automation: Option<UIAutomation>,
    last: Option<(CaptionObservation, Instant)>,
}
impl CaptionProbe {
    pub(super) fn new() -> Self {
        Self {
            automation: None,
            last: None,
        }
    }
    pub(super) fn measure(&mut self, hwnd: HWND) -> Option<CaptionObservation> {
        let window = get_window_rect(hwnd)?;
        let frame = extended_frame_bounds(hwnd).unwrap_or(window);
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if let Some((observation, checked)) = self.last
            && checked.elapsed() < Duration::from_secs(2)
            && observation.bottom(hwnd, window, frame).is_some()
        {
            return Some(observation);
        }
        if self.automation.is_none() {
            self.automation = UIAutomation::new()
                .or_else(|_| UIAutomation::new_direct())
                .ok();
        }
        let automation = self.automation.as_ref()?;
        let root = automation
            .element_from_handle(Handle::from(hwnd.0 as isize))
            .ok()?;
        let buttons = automation
            .create_property_condition(
                UIProperty::ControlType,
                (ControlType::Button as i32).into(),
                None,
            )
            .ok()?;
        let visible = automation
            .create_property_condition(UIProperty::IsOffscreen, false.into(), None)
            .ok()?;
        let condition = automation.create_and_condition(buttons, visible).ok()?;
        let request = automation.create_cache_request().ok()?;
        request.set_tree_scope(TreeScope::Element).ok()?;
        request.set_element_mode(ElementMode::Full).ok()?;
        request
            .set_tree_filter(automation.create_true_condition().ok()?)
            .ok()?;
        for property in [
            UIProperty::Name,
            UIProperty::BoundingRectangle,
            UIProperty::IsOffscreen,
        ] {
            request.add_property(property).ok()?;
        }
        let elements = root
            .find_all_build_cache(TreeScope::Subtree, &condition, &request)
            .ok()?;
        let mut controls = Vec::new();
        for element in elements {
            let Ok(name) = element.get_cached_name() else {
                continue;
            };
            let Some(role) = caption_role(&name) else {
                continue;
            };
            if element.is_cached_offscreen().unwrap_or(true) {
                continue;
            }
            if let Ok(rect) = element.get_cached_bounding_rectangle() {
                controls.push((
                    role,
                    RectI {
                        left: rect.get_left(),
                        top: rect.get_top(),
                        right: rect.get_right(),
                        bottom: rect.get_bottom(),
                    },
                ));
            }
        }
        let bottom = select_caption_bottom(frame, dpi, &controls)?;
        // Results from a layout/DPI transition must never change the anchor.
        if get_window_rect(hwnd) != Some(window)
            || extended_frame_bounds(hwnd).unwrap_or(window) != frame
            || unsafe { GetDpiForWindow(hwnd) } != dpi
        {
            return None;
        }
        let observation = CaptionObservation::new(hwnd, window, frame, dpi, bottom);
        self.last = Some((observation, Instant::now()));
        Some(observation)
    }
}

fn caption_role(name: &str) -> Option<u8> {
    match name.trim().to_lowercase().as_str() {
        "最小化" | "minimize" => Some(1),
        "最大化" | "maximize" | "元に戻す" | "restore" | "restore down" => Some(2),
        "閉じる" | "close" => Some(3),
        _ => None,
    }
}

fn select_caption_bottom(frame: RectI, dpi: u32, controls: &[(u8, RectI)]) -> Option<i32> {
    if !(96..=768).contains(&dpi) {
        return None;
    }
    let scale = dpi as f32 / 96.0;
    let max_height = (64.0 * scale).ceil() as i32;
    let tolerance = scale.ceil() as i32;
    let valid = |rect: RectI| {
        rect.width() > 0
            && rect.height() >= (12.0 * scale) as i32
            && rect.left >= frame.center_x()
            && rect.right <= frame.right + tolerance
            && rect.top >= frame.top - tolerance
            && rect.top <= frame.top + (12.0 * scale) as i32
            && rect.bottom <= frame.top + max_height
    };
    let mut rows = Vec::new();
    for &(role, rect) in controls {
        if !valid(rect) {
            continue;
        }
        if controls.iter().any(|&(other, candidate)| {
            other != role
                && valid(candidate)
                && candidate.left != rect.left
                && (candidate.top - rect.top).abs() <= tolerance
                && (candidate.bottom - rect.bottom).abs() <= tolerance
        }) {
            rows.push(rect.bottom);
        }
    }
    let bottom = *rows.iter().min()?;
    // Two inconsistent caption rows are ambiguous, not permission to guess.
    rows.iter()
        .all(|row| (*row - bottom).abs() <= tolerance)
        .then_some(bottom)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_custom_caption_row_overrides_a_taller_native_band() {
        for dpi in [96, 120, 144, 168, 192, 288] {
            let scale = dpi as f32 / 96.0;
            for height in [30, 46] {
                let frame = RectI {
                    left: -1800,
                    top: 70,
                    right: 0,
                    bottom: 1200,
                };
                let bottom = frame.top + (height as f32 * scale).round() as i32;
                let controls = [
                    (
                        1,
                        RectI {
                            left: -280,
                            top: frame.top,
                            right: -180,
                            bottom,
                        },
                    ),
                    (
                        3,
                        RectI {
                            left: -150,
                            top: frame.top,
                            right: 0,
                            bottom,
                        },
                    ),
                ];
                assert_eq!(select_caption_bottom(frame, dpi, &controls), Some(bottom));
            }
        }
    }
    #[test]
    fn toolbar_close_and_ambiguous_caption_rows_cannot_reanchor_the_island() {
        let frame = RectI {
            left: 0,
            top: 0,
            right: 1000,
            bottom: 900,
        };
        let a = RectI {
            left: 850,
            top: 0,
            right: 900,
            bottom: 30,
        };
        let b = RectI {
            left: 950,
            right: 1000,
            ..a
        };
        assert_eq!(
            select_caption_bottom(
                frame,
                96,
                &[
                    (1, a),
                    (
                        3,
                        RectI {
                            top: 80,
                            bottom: 110,
                            ..b
                        }
                    )
                ]
            ),
            None
        );
        assert_eq!(select_caption_bottom(frame, 96, &[(1, a), (1, b)]), None);
        assert_eq!(caption_role("チャットを閉じる"), None);
    }
}
