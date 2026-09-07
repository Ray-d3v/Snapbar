use std::{thread, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use std::ffi::c_void;
use uiautomation::types::{
    ControlType, ElementMode, Handle, Point, Rect as UiRect, TreeScope, UIProperty,
};
use uiautomation::{UIAutomation, UIElement};
use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
    UI::WindowsAndMessaging::{GetWindowRect, IsIconic, IsWindow},
};

use super::{ScreenRect, content_detector::PixelRect};

const PROVIDER_WARMUP_DELAY: Duration = Duration::from_millis(50);
const STABILITY_DELAY: Duration = Duration::from_millis(35);
const RECT_STABILITY_TOLERANCE: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WindowGeometry {
    screen_left: i32,
    screen_top: i32,
    screen_width: u32,
    screen_height: u32,
    image_width: u32,
    image_height: u32,
}

impl WindowGeometry {
    pub(super) fn from_hwnd_dimensions(
        target_id: u32,
        image_width: u32,
        image_height: u32,
    ) -> Result<Self> {
        let _physical = crate::dpi::PhysicalPixels::enter();
        let hwnd = HWND(target_id as usize as *mut c_void);
        let mut rect = RECT::default();
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() || IsIconic(hwnd).as_bool() {
                return Err(anyhow!("撮影対象のTeamsウィンドウを確認できませんでした"));
            }
            if DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                (&mut rect as *mut RECT).cast(),
                std::mem::size_of::<RECT>() as u32,
            )
            .is_err()
            {
                GetWindowRect(hwnd, &mut rect)
                    .context("Teamsウィンドウの位置を取得できませんでした")?;
            }
        }
        if rect.right <= rect.left
            || rect.bottom <= rect.top
            || image_width == 0
            || image_height == 0
        {
            return Err(anyhow!("Teamsウィンドウのサイズが不正です"));
        }
        Ok(Self::from_screen_rect(
            ScreenRect {
                x: rect.left,
                y: rect.top,
                width: (i64::from(rect.right) - i64::from(rect.left)) as u32,
                height: (i64::from(rect.bottom) - i64::from(rect.top)) as u32,
            },
            image_width,
            image_height,
        ))
    }

    pub(super) fn from_screen_rect(rect: ScreenRect, image_width: u32, image_height: u32) -> Self {
        Self {
            screen_left: rect.x,
            screen_top: rect.y,
            screen_width: rect.width,
            screen_height: rect.height,
            image_width,
            image_height,
        }
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

        let raw_area =
            u64::try_from(raw_right - raw_left).ok()? * u64::try_from(raw_bottom - raw_top).ok()?;
        let intersection_area = u64::try_from(intersection_right - intersection_left).ok()?
            * u64::try_from(intersection_bottom - intersection_top).ok()?;
        if intersection_area.saturating_mul(100) < raw_area.saturating_mul(98) {
            return None;
        }

        self.map_ui_rect(rect)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AuthoritativeCandidate {
    rect: PixelRect,
    rank: u8,
}

// UIA clients and selected identities are kept only on LayoutResolver's MTA.
// Every fast-path use calls BuildUpdatedCache: cached values are NEVER evidence.
use super::layout::Request;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc::SyncSender,
};
use std::time::Instant;
use uiautomation::{
    core::UICacheRequest,
    events::{
        CustomPropertyChangedEventHandler, CustomStructureChangedEventHandler,
        UIPropertyChangedEventHandler, UIStructureChangeEventHandler,
    },
    types::StructureChangeType,
    variants::Variant,
};

struct Invalidation {
    revision: Arc<AtomicU64>,
    wake: SyncSender<Request>,
}
impl Invalidation {
    fn signal(&self) {
        self.revision.fetch_add(1, Ordering::AcqRel);
        let _ = self.wake.try_send(Request::Changed);
    }
}
impl CustomStructureChangedEventHandler for Invalidation {
    fn handle(
        &self,
        _: &UIElement,
        _: StructureChangeType,
        _: Option<&[i32]>,
    ) -> uiautomation::Result<()> {
        self.signal();
        Ok(())
    }
}
impl CustomPropertyChangedEventHandler for Invalidation {
    fn handle(&self, _: &UIElement, _: UIProperty, _: Variant) -> uiautomation::Result<()> {
        self.signal();
        Ok(())
    }
}

struct LocatedContent {
    element: UIElement,
    candidate: AuthoritativeCandidate,
    geometry: WindowGeometry,
    revision: u64,
}

pub(super) struct ContentLocator {
    automation: UIAutomation,
    target: u32,
    root: UIElement,
    request: UICacheRequest,
    structure: UIStructureChangeEventHandler,
    properties: UIPropertyChangedEventHandler,
    subscribed: bool,
    revision: Arc<AtomicU64>,
    selected: Option<LocatedContent>,
    last_discovery: Option<Instant>,
}

impl ContentLocator {
    pub(super) fn new(target: u32, wake: SyncSender<Request>) -> Result<Self> {
        let automation = UIAutomation::new().or_else(|_| UIAutomation::new_direct())?;
        let root = automation.element_from_handle(Handle::from(target as isize))?;
        let request = automation.create_cache_request()?;
        request.set_tree_scope(TreeScope::Element)?;
        request.set_tree_filter(automation.create_true_condition()?)?;
        request.set_element_mode(ElementMode::Full)?;
        for property in [
            UIProperty::Name,
            UIProperty::ControlType,
            UIProperty::IsOffscreen,
            UIProperty::BoundingRectangle,
        ] {
            request.add_property(property)?;
        }
        let revision = Arc::new(AtomicU64::new(1));
        let structure = UIStructureChangeEventHandler::from(Invalidation {
            revision: revision.clone(),
            wake: wake.clone(),
        });
        let properties = UIPropertyChangedEventHandler::from(Invalidation {
            revision: revision.clone(),
            wake,
        });
        let structure_ok = automation
            .add_structure_changed_event_handler(&root, TreeScope::Subtree, None, &structure)
            .is_ok();
        let properties_ok = automation
            .add_property_changed_event_handler(
                &root,
                TreeScope::Subtree,
                None,
                &properties,
                &[UIProperty::IsOffscreen],
            )
            .is_ok();
        Ok(Self {
            automation,
            target,
            root,
            request,
            structure,
            properties,
            subscribed: structure_ok && properties_ok,
            revision,
            selected: None,
            last_discovery: None,
        })
    }

    pub(super) fn is_current(&self, revision: u64) -> bool {
        self.revision.load(Ordering::Acquire) == revision
    }

    pub(super) fn locate(
        &mut self,
        geometry: WindowGeometry,
        background: bool,
    ) -> Result<(PixelRect, u64)> {
        let revision = self.revision.load(Ordering::Acquire);
        let watchdog_due = background
            && self
                .last_discovery
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(2));
        if !requires_discovery(
            self.subscribed,
            self.selected.as_ref().is_some_and(|located| {
                located.geometry == geometry && located.revision == revision
            }),
            watchdog_due,
        ) && let Some(located) = self.selected.as_ref()
        {
            // One cross-process element refresh rather than two complete
            // subtree enumerations and an unconditional 35ms sleep.
            let raw: &windows::Win32::UI::Accessibility::IUIAutomationElement =
                located.element.as_ref();
            let refreshed =
                unsafe { raw.BuildUpdatedCache(self.request.as_ref()) }.map(UIElement::from);
            if let Ok(refreshed) = refreshed
                && authoritative_candidate_from_element(&refreshed, geometry)
                    == Some(located.candidate)
                && self.is_current(revision)
            {
                return Ok((located.candidate.rect, revision));
            }
            // A live-property mismatch is a change even if a provider
            // dropped its event. Do not continue using the old crop.
            self.revision.fetch_add(1, Ordering::AcqRel);
        }
        self.discover(geometry)
    }

    fn discover(&mut self, geometry: WindowGeometry) -> Result<(PixelRect, u64)> {
        let revision = self.revision.load(Ordering::Acquire);
        let result = (|| {
            let mut first = scan_authoritative_element(&self.automation, self.target, geometry)?;
            if first.is_none() {
                let _ = self
                    .automation
                    .element_from_point(geometry.sample_point(0.50, 0.55));
                thread::sleep(PROVIDER_WARMUP_DELAY);
                first = scan_authoritative_element(&self.automation, self.target, geometry)?;
            }
            let (first_element, first) =
                first.ok_or_else(|| anyhow!("Teamsの共有コンテンツ要素を確認できませんでした"))?;
            thread::sleep(STABILITY_DELAY);
            let (element, candidate) =
                scan_authoritative_element(&self.automation, self.target, geometry)?
                    .ok_or_else(|| anyhow!("Teamsの共有範囲が確定していません"))?;
            if !rects_are_stable(first.rect, candidate.rect, RECT_STABILITY_TOLERANCE)
                || !self.automation.compare_elements(&first_element, &element)?
                || !self.is_current(revision)
            {
                return Err(anyhow!("Teamsの共有範囲を更新中です"));
            }
            let changed = self.selected.as_ref().is_some_and(|old| {
                old.geometry != geometry
                    || old.candidate != candidate
                    || !self
                        .automation
                        .compare_elements(&old.element, &element)
                        .unwrap_or(false)
            });
            let next_revision = revision + u64::from(changed);
            self.revision
                .compare_exchange(revision, next_revision, Ordering::AcqRel, Ordering::Acquire)
                .map_err(|_| anyhow!("共有範囲の確認中にTeamsの構造が変わりました"))?;
            let revision = next_revision;
            self.selected = Some(LocatedContent {
                element,
                candidate,
                geometry,
                revision,
            });
            Ok((candidate.rect, revision))
        })();
        self.last_discovery = Some(Instant::now());
        if result.is_err() {
            self.selected = None;
            // A failed discovery must invalidate pixels authorized by its old
            // selection, including failures in the very first provider call.
            self.revision.fetch_add(1, Ordering::AcqRel);
        }
        result
    }
}
fn requires_discovery(subscribed: bool, unchanged: bool, watchdog_due: bool) -> bool {
    !subscribed || !unchanged || watchdog_due
}

impl Drop for ContentLocator {
    fn drop(&mut self) {
        let _ = self
            .automation
            .remove_structure_changed_event_handler(&self.root, &self.structure);
        let _ = self
            .automation
            .remove_property_changed_event_handler(&self.root, &self.properties);
    }
}

fn scan_authoritative_element(
    automation: &UIAutomation,
    target_id: u32,
    geometry: WindowGeometry,
) -> Result<Option<(UIElement, AuthoritativeCandidate)>> {
    let root = automation
        .element_from_handle(Handle::from(target_id as isize))
        .context("TeamsウィンドウのUI Automationルートを取得できませんでした")?;
    let condition = automation
        .create_true_condition()
        .context("UI Automationの検索条件を作成できませんでした")?;
    let snapshot = || -> uiautomation::Result<Vec<UIElement>> {
        // Match the existing exclusions before marshaling result objects. Each
        // scan gets new values; no rectangle or element survives this scan.
        let visible =
            automation.create_property_condition(UIProperty::IsOffscreen, false.into(), None)?;
        let named = automation.create_not_condition(automation.create_property_condition(
            UIProperty::Name,
            "".into(),
            None,
        )?)?;
        let relevant = automation.create_and_condition(visible, named)?;
        let request = automation.create_cache_request()?;
        request.set_tree_scope(TreeScope::Element)?;
        request.set_tree_filter(automation.create_true_condition()?)?;
        request.set_element_mode(ElementMode::Full)?;
        for property in [
            UIProperty::Name,
            UIProperty::ControlType,
            UIProperty::IsOffscreen,
            UIProperty::BoundingRectangle,
        ] {
            request.add_property(property)?;
        }
        root.find_all_build_cache(TreeScope::Subtree, &relevant, &request)
    };
    // Retain compatibility with providers that cannot perform a bulk request.
    let elements = snapshot()
        .or_else(|_| root.find_all(TreeScope::Subtree, &condition))
        .context("TeamsのUI Automationツリーを走査できませんでした")?;

    let mut candidates = Vec::new();
    let mut identities = Vec::new();
    for element in elements {
        let Some(candidate) = authoritative_candidate_from_element(&element, geometry) else {
            continue;
        };
        insert_or_replace_candidate(&mut candidates, candidate);
        identities.push((element, candidate));
    }

    let selected = select_unique_authoritative_candidate(&candidates);
    Ok(selected.and_then(|candidate| {
        identities
            .into_iter()
            .find(|(_, value)| *value == candidate)
    }))
}

fn authoritative_candidate_from_element(
    element: &UIElement,
    geometry: WindowGeometry,
) -> Option<AuthoritativeCandidate> {
    if element
        .is_cached_offscreen()
        .or_else(|_| element.is_offscreen())
        .unwrap_or(true)
    {
        return None;
    }

    let name_rank = authoritative_name_rank(
        &element
            .get_cached_name()
            .or_else(|_| element.get_name())
            .ok()?,
    )?;
    let control_rank = authoritative_control_rank(
        element
            .get_cached_control_type()
            .or_else(|_| element.get_control_type())
            .ok()?,
    )?;
    let rect = geometry.map_ui_rect_strict(
        element
            .get_cached_bounding_rectangle()
            .or_else(|_| element.get_bounding_rectangle())
            .ok()?,
    )?;
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
        ControlType::Pane | ControlType::Custom | ControlType::Group | ControlType::Image => {
            Some(3)
        }
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
    #[test]
    fn unchanged_identity_does_not_require_full_discovery_on_capture() {
        assert!(!super::requires_discovery(true, true, false));
    }
    #[test]
    fn changes_lost_subscriptions_and_watchdog_force_discovery() {
        assert!(super::requires_discovery(true, false, false));
        assert!(super::requires_discovery(false, true, false));
        assert!(super::requires_discovery(true, true, true));
    }
}
