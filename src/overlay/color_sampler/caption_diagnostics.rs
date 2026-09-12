//! Read-only caption diagnostics. Never supplies geometry to the overlay.
//! Runs on the existing color worker, before the normal authoritative probe.
use std::{
    collections::HashSet,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use uiautomation::{
    UIAutomation, UIElement,
    core::UICacheRequest,
    types::{ElementMode, Handle, TreeScope, UIProperty},
};
use windows::Win32::{
    Foundation::HWND,
    UI::{
        HiDpi::GetDpiForWindow,
        WindowsAndMessaging::{IsIconic, IsWindowVisible},
    },
};

use super::super::{RectI, extended_frame_bounds, get_window_rect};
use crate::automation::AutomationClient;

const ROW_IDS: [&str; 3] = ["indicators", "horizontalMiddleEnd", "horizontalEnd"];
const MIN_INTERVAL: Duration = Duration::from_secs(5);
const REPEAT_INTERVAL: Duration = Duration::from_secs(30);
const WALK_BUDGET: Duration = Duration::from_millis(500);
const MAX_STEPS: usize = 192;
const MAX_ROWS: usize = 6;
const MAX_DEPTH: usize = 64;
const MAX_CHILDREN: usize = 32;
const MAX_NODES: usize = 160;
const MAX_HEADER_SCAN: usize = 2048;
const MAX_HEADER_NODES: usize = 64;
const CACHED_READ_BUDGET: Duration = Duration::from_millis(100);
const PROPERTIES: [UIProperty; 4] = [
    UIProperty::AutomationId,
    UIProperty::ControlType,
    UIProperty::IsOffscreen,
    UIProperty::BoundingRectangle,
];
static NEXT_TRACE: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct TraceThrottle {
    last: Option<(isize, bool, Instant)>,
}

impl TraceThrottle {
    fn allow(&mut self, target: isize, ready: bool, now: Instant) -> bool {
        if let Some((old_target, old_ready, last)) = self.last {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < MIN_INTERVAL
                || (old_target == target && old_ready == ready && elapsed < REPEAT_INTERVAL)
            {
                return false;
            }
        }
        self.last = Some((target, ready, now));
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Geometry {
    window: RectI,
    frame: RectI,
    dpi: u32,
}

impl Geometry {
    fn read(hwnd: HWND) -> Option<Self> {
        if !unsafe { IsWindowVisible(hwnd).as_bool() } || unsafe { IsIconic(hwnd).as_bool() } {
            return None;
        }
        let window = get_window_rect(hwnd)?;
        Some(Self {
            window,
            frame: extended_frame_bounds(hwnd).unwrap_or(window),
            dpi: unsafe { GetDpiForWindow(hwnd) },
        })
    }
}

struct WalkState {
    started: Instant,
    steps: usize,
    nodes: usize,
    truncated: bool,
    navigation_stops: usize,
    rows_same: Option<bool>,
    header_scanned: usize,
    header_nodes: usize,
    depth_limits: usize,
}

impl WalkState {
    fn new(started: Instant) -> Self {
        Self {
            started,
            steps: 0,
            nodes: 0,
            truncated: false,
            navigation_stops: 0,
            rows_same: None,
            header_scanned: 0,
            header_nodes: 0,
            depth_limits: 0,
        }
    }

    fn step_at(&mut self, now: Instant) -> Result<(), &'static str> {
        // Cooperative limits only: an individual COM call cannot be cancelled.
        if self.steps >= MAX_STEPS
            || self.nodes >= MAX_NODES
            || now.saturating_duration_since(self.started) >= WALK_BUDGET
        {
            self.truncated = true;
            return Err("walk_budget");
        }
        self.steps += 1;
        Ok(())
    }

    fn step(&mut self) -> Result<(), &'static str> {
        self.step_at(Instant::now())
    }
}

#[derive(Default)]
pub(super) struct CaptionTrace {
    throttle: TraceThrottle,
    automation: Option<AutomationClient>,
}

impl CaptionTrace {
    pub(super) fn record_if_due(&mut self, hwnd: HWND, previous_probe_ready: bool) {
        if !crate::diagnostics::enabled() {
            return;
        }
        let Some(before) = Geometry::read(hwnd) else {
            return;
        };
        let started = Instant::now();
        if !self
            .throttle
            .allow(hwnd.0 as isize, previous_probe_ready, started)
        {
            return;
        }
        let trace = NEXT_TRACE.fetch_add(1, Ordering::Relaxed);
        crate::diagnostics::log(format_args!(
            "caption_tree_begin schema=2 trace={trace} target={} previous_probe_ready={previous_probe_ready} geometry={before:?}",
            hwnd.0 as isize
        ));
        let mut state = WalkState::new(started);
        if self.automation.is_none() {
            self.automation = AutomationClient::new().ok();
        }
        let result = match self.automation.as_ref() {
            Some(automation) => collect(automation, hwnd, before, trace, &mut state),
            None => Err("automation_unavailable"),
        };
        let after = Geometry::read(hwnd);
        crate::diagnostics::log(format_args!(
            "caption_tree_end trace={trace} reason={:?} elapsed_ms={} steps={} nodes={} truncated={} navigation_end_or_error={} rows_same={:?} geometry_same={} header_scanned={} header_nodes={} depth_limits={} after={after:?} atomic=false",
            result.err(),
            started.elapsed().as_millis(),
            state.steps,
            state.nodes,
            state.truncated,
            state.navigation_stops,
            state.rows_same,
            after == Some(before),
            state.header_scanned,
            state.header_nodes,
            state.depth_limits
        ));
    }
}

fn diagnostic_cache(
    automation: &UIAutomation,
    mode: ElementMode,
) -> Result<UICacheRequest, &'static str> {
    let cache = automation.create_cache_request().map_err(|_| "cache")?;
    cache
        .set_tree_scope(TreeScope::Element)
        .map_err(|_| "cache")?;
    cache.set_element_mode(mode).map_err(|_| "cache")?;
    cache
        .set_tree_filter(automation.create_true_condition().map_err(|_| "cache")?)
        .map_err(|_| "cache")?;
    // Never request Name, ValuePattern, TextPattern, pixels or clipboard data.
    for property in PROPERTIES {
        cache.add_property(property).map_err(|_| "cache")?;
    }
    Ok(cache)
}

fn cached_rect(element: &UIElement) -> Option<RectI> {
    let rect = element.get_cached_bounding_rectangle().ok()?;
    Some(RectI {
        left: rect.get_left(),
        top: rect.get_top(),
        right: rect.get_right(),
        bottom: rect.get_bottom(),
    })
}

fn current_rect(element: &UIElement) -> Option<RectI> {
    let rect = element.get_bounding_rectangle().ok()?;
    Some(RectI {
        left: rect.get_left(),
        top: rect.get_top(),
        right: rect.get_right(),
        bottom: rect.get_bottom(),
    })
}

fn header_intersects(frame: RectI, bottom: i32, rect: Option<RectI>) -> bool {
    rect.is_some_and(|rect| {
        rect.width() > 0
            && rect.height() > 0
            && rect.right > frame.left
            && rect.left < frame.right
            && rect.bottom > frame.top
            && rect.top < bottom
    })
}

fn diagnostic_id(id: &str) -> String {
    id.chars()
        .take(80)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn log_node(
    trace: u64,
    relation: &str,
    role: usize,
    depth: usize,
    element: &UIElement,
    state: &mut WalkState,
) {
    state.nodes += 1;
    let id = element
        .get_cached_automation_id()
        .ok()
        .map(|id| diagnostic_id(&id));
    let kind = element
        .get_cached_control_type()
        .ok()
        .map(|kind| kind as i32);
    let offscreen = element.is_cached_offscreen().ok();
    let rect = cached_rect(element);
    crate::diagnostics::log(format_args!(
        "caption_tree_node trace={trace} relation={relation} role={role} depth={depth} id={id:?} kind={kind:?} offscreen={offscreen:?} rect={rect:?}"
    ));
}

// Prefer bounded elements entirely above the meeting row to full-window
// wrapper elements. This ordering is diagnostic only, never an anchor rule.
fn header_priority(frame: RectI, row_top: i32, rect: RectI) -> (bool, i32, i32) {
    (
        rect.top < frame.top || rect.bottom > row_top,
        rect.top,
        rect.bottom,
    )
}

fn record_scoped_header(
    automation: &UIAutomation,
    root: &UIElement,
    frame: RectI,
    row_top: i32,
    trace: u64,
    state: &mut WalkState,
) -> Result<(), &'static str> {
    let cache = diagnostic_cache(automation, ElementMode::None)?;
    let condition = automation
        .create_true_condition()
        .map_err(|_| "condition")?;
    state.step()?;
    let started = Instant::now();
    // One root-scoped bulk query, with an Element-only cache (not a subtree
    // cache per match). No ControlType/Name filter can hide a banner wrapper.
    let elements = root
        .find_all_build_cache(TreeScope::Descendants, &condition, &cache)
        .map_err(|_| "header_query")?;
    let query_ms = started.elapsed().as_millis();
    let returned = elements.len();
    let local_start = Instant::now();
    let mut candidates = Vec::new();
    let mut stop = None;
    for element in elements.into_iter().take(MAX_HEADER_SCAN) {
        if local_start.elapsed() >= CACHED_READ_BUDGET {
            stop = Some("cached_read_budget");
            break;
        }
        state.header_scanned += 1;
        let rect = cached_rect(&element);
        if element.is_cached_offscreen().ok() == Some(false)
            && header_intersects(frame, row_top, rect)
            && let Some(rect) = rect
        {
            candidates.push((header_priority(frame, row_top, rect), element));
        }
    }
    if returned > MAX_HEADER_SCAN {
        stop = Some("cached_scan_limit");
    }
    candidates.sort_by_key(|(priority, _)| *priority);
    let candidate_count = candidates.len();
    if candidate_count > MAX_HEADER_NODES {
        stop = Some("header_log_limit");
    }
    // These are cache-only references. Preserve already-returned evidence even
    // if the preceding provider call exceeded the cooperative RPC budget.
    for (_, element) in candidates.into_iter().take(MAX_HEADER_NODES) {
        log_node(trace, "scoped_header", 0, 0, &element, state);
        state.header_nodes += 1;
    }
    state.truncated |= stop.is_some();
    crate::diagnostics::log(format_args!(
        "caption_tree_header trace={trace} scope=target_descendants ancestry_independent=true returned={returned} scanned={} candidates={candidate_count} logged={} query_ms={query_ms} cached_read_ms={} stop={stop:?}",
        state.header_scanned,
        state.header_nodes,
        local_start.elapsed().as_millis()
    ));
    Ok(())
}

// The root at exactly max_depth must be checked before declining another
// parent fetch. The old 0..12 loop discarded that root and every deeper path.
fn verified_path<T: Clone>(
    start: T,
    max_depth: usize,
    mut is_root: impl FnMut(&T) -> Result<bool, &'static str>,
    mut parent: impl FnMut(&T) -> Result<T, &'static str>,
    mut step: impl FnMut() -> Result<(), &'static str>,
) -> Result<Vec<T>, &'static str> {
    let mut path = Vec::new();
    let mut node = start;
    loop {
        step()?;
        if is_root(&node)? {
            return Ok(path);
        }
        if path.len() == max_depth {
            return Err("depth_limit");
        }
        step()?;
        node = parent(&node)?;
        path.push(node.clone());
    }
}

fn collect(
    automation: &UIAutomation,
    hwnd: HWND,
    geometry: Geometry,
    trace: u64,
    state: &mut WalkState,
) -> Result<(), &'static str> {
    let cache = diagnostic_cache(automation, ElementMode::Full)?;
    state.step()?;
    let root = automation
        .element_from_handle_build_cache(Handle::from(hwnd.0 as isize), &cache)
        .map_err(|_| "root")?;
    let walker = automation.get_raw_view_walker().map_err(|_| "walker")?;
    let mut condition = automation
        .create_false_condition()
        .map_err(|_| "condition")?;
    for id in ROW_IDS {
        let part = automation
            .create_property_condition(UIProperty::AutomationId, id.into(), None)
            .map_err(|_| "condition")?;
        condition = automation
            .create_or_condition(condition, part)
            .map_err(|_| "condition")?;
    }
    state.step()?;
    let rows = root
        .find_all_build_cache(TreeScope::Descendants, &condition, &cache)
        .map_err(|_| "rows_query")?;
    state.truncated |= rows.len() > MAX_ROWS;
    let rows: Vec<_> = rows.into_iter().take(MAX_ROWS).collect();
    let header_bottom = rows
        .iter()
        .filter_map(cached_rect)
        .map(|rect| rect.bottom.min(geometry.frame.bottom))
        .max()
        .unwrap_or(geometry.frame.top);
    crate::diagnostics::log(format_args!(
        "caption_tree_rows trace={trace} count={} header_bottom={header_bottom}",
        rows.len()
    ));
    if rows.is_empty() {
        return Err("rows_missing");
    }
    // Record every row before spending any budget on optional ancestry.
    for row in &rows {
        let id = row.get_cached_automation_id().unwrap_or_default();
        let role = ROW_IDS.iter().position(|known| *known == id).unwrap_or(3) + 1;
        log_node(trace, "row", role, 0, row, state);
    }
    let row_top = rows
        .iter()
        .filter(|row| row.is_cached_offscreen().ok() == Some(false))
        .filter_map(cached_rect)
        .filter(|rect| rect.width() > 0 && rect.height() > 0 && rect.top > geometry.frame.top)
        .map(|rect| rect.top.min(geometry.frame.bottom))
        .min();
    if let Some(row_top) = row_top {
        record_scoped_header(automation, &root, geometry.frame, row_top, trace, state)?;
    }
    let mut parents = Vec::new();
    let mut seen = HashSet::new();
    for row in &rows {
        let id = row.get_cached_automation_id().unwrap_or_default();
        let role = ROW_IDS.iter().position(|known| *known == id).unwrap_or(3) + 1;
        let path = verified_path(
            row.clone(),
            MAX_DEPTH,
            |node| {
                automation
                    .compare_elements(node, &root)
                    .map_err(|_| "compare")
            },
            |node| {
                walker
                    .get_parent_build_cache(node, &cache)
                    .map_err(|_| "parent")
            },
            || state.step(),
        );
        let path = match path {
            Ok(path) => path,
            Err(reason) => {
                state.truncated = true;
                state.depth_limits += usize::from(reason == "depth_limit");
                crate::diagnostics::log(format_args!(
                    "caption_tree_path trace={trace} role={role} complete=false reason={reason} max_depth={MAX_DEPTH}"
                ));
                if reason == "walk_budget" {
                    return Err(reason);
                }
                continue;
            }
        };
        crate::diagnostics::log(format_args!(
            "caption_tree_path trace={trace} role={role} complete=true depth={}",
            path.len()
        ));
        // Only a proven path to the exact selected root can authorize ancestor
        // logging/child navigation. Flat header evidence does not weaken this.
        for (index, parent) in path.into_iter().enumerate() {
            state.step()?;
            let runtime_id = parent.get_runtime_id().map_err(|_| "runtime_id")?;
            if seen.insert(runtime_id) {
                log_node(trace, "ancestor", role, index + 1, &parent, state);
                parents.push((role, index + 1, parent));
            }
        }
    }
    for (role, depth, parent) in parents.into_iter().rev() {
        state.step()?;
        let mut child = walker.get_first_child_build_cache(&parent, &cache).ok();
        if child.is_none() {
            state.navigation_stops += 1;
        }
        for _ in 0..MAX_CHILDREN {
            let Some(element) = child.take() else {
                break;
            };
            state.step()?;
            if header_intersects(geometry.frame, header_bottom, cached_rect(&element)) {
                log_node(trace, "child", role, depth, &element, state);
            }
            child = walker.get_next_sibling_build_cache(&element, &cache).ok();
            if child.is_none() {
                state.navigation_stops += 1;
            }
        }
        state.truncated |= child.is_some();
    }
    let mut same = true;
    for row in &rows {
        state.step()?;
        let rect = current_rect(row);
        let offscreen = row.is_offscreen().ok();
        same &= rect.is_some()
            && rect == cached_rect(row)
            && offscreen.is_some()
            && offscreen == row.is_cached_offscreen().ok();
    }
    state.rows_same = Some(same);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_success_or_failure_is_rate_limited() {
        for ready in [false, true] {
            let start = Instant::now();
            let mut throttle = TraceThrottle::default();
            assert!(throttle.allow(1, ready, start));
            assert!(!throttle.allow(1, ready, start + Duration::from_secs(29)));
            assert!(throttle.allow(1, ready, start + REPEAT_INTERVAL));
        }
    }

    #[test]
    fn transitions_do_not_bypass_the_minimum_interval() {
        let start = Instant::now();
        let mut throttle = TraceThrottle::default();
        assert!(throttle.allow(1, true, start));
        assert!(!throttle.allow(2, false, start + Duration::from_secs(1)));
        assert!(throttle.allow(2, false, start + MIN_INTERVAL));
        assert!(!throttle.allow(2, true, start + MIN_INTERVAL));
        assert!(throttle.allow(2, true, start + MIN_INTERVAL * 2));
    }

    #[test]
    fn walk_budget_limits_steps_and_elapsed_time_without_sleeping() {
        let start = Instant::now();
        let mut state = WalkState::new(start);
        for _ in 0..MAX_STEPS {
            assert!(state.step_at(start).is_ok());
        }
        assert!(state.step_at(start).is_err());
        assert!(state.truncated);
        let mut state = WalkState::new(start);
        assert!(state.step_at(start + WALK_BUDGET).is_err());
    }

    #[test]
    fn log_node_budget_is_independent_of_navigation_steps() {
        let start = Instant::now();
        let mut state = WalkState::new(start);
        state.nodes = MAX_NODES;
        assert!(state.step_at(start).is_err());
        assert_eq!(state.steps, 0);
    }

    #[test]
    fn diagnostic_identifiers_are_bounded_and_cannot_inject_log_lines() {
        assert_eq!(diagnostic_id("banner\r\n\tclose"), "banner   close");
        assert_eq!(diagnostic_id(&"界".repeat(100)).chars().count(), 80);
        assert_eq!(diagnostic_id("horizontalEnd"), "horizontalEnd");
    }

    #[test]
    fn header_filter_includes_the_reported_gap_but_not_meeting_content() {
        let frame = RectI {
            left: 409,
            top: 191,
            right: 2313,
            bottom: 1349,
        };
        let banner = RectI {
            left: 421,
            top: 236,
            right: 2293,
            bottom: 288,
        };
        assert!(header_intersects(frame, 375, Some(banner)));
        assert!(!header_intersects(
            frame,
            375,
            Some(RectI {
                top: 375,
                bottom: 900,
                ..banner
            })
        ));
        assert!(!header_intersects(frame, 375, None));
        let moved = frame.offset(-3000, -2000);
        assert!(header_intersects(
            moved,
            -1625,
            Some(banner.offset(-3000, -2000))
        ));
    }

    #[test]
    fn ancestry_checks_the_root_at_the_exact_depth_boundary() {
        let path = verified_path(
            0,
            12,
            |node| Ok(*node == 12),
            |node| {
                assert!(*node < 12, "must not step above the selected root");
                Ok(*node + 1)
            },
            || Ok(()),
        )
        .unwrap();
        assert_eq!(path, (1..=12).collect::<Vec<_>>());
    }

    #[test]
    fn deeper_teams_wrappers_do_not_discard_a_verified_path() {
        for depth in [13, 20, 32, MAX_DEPTH] {
            let path = verified_path(
                0,
                MAX_DEPTH,
                |node| Ok(*node == depth),
                |node| Ok(*node + 1),
                || Ok(()),
            )
            .unwrap();
            assert_eq!(path.len(), depth);
            assert_eq!(path.last(), Some(&depth));
        }
    }

    #[test]
    fn unrelated_roots_and_cycles_still_fail_closed_at_the_depth_limit() {
        let result = verified_path(0, 12, |_| Ok(false), |node| Ok((*node + 1) % 3), || Ok(()));
        assert_eq!(result, Err("depth_limit"));
    }

    #[test]
    fn a_walk_budget_error_never_returns_an_unverified_parent_path() {
        let start = Instant::now();
        let mut state = WalkState::new(start);
        state.header_nodes = 3;
        state.nodes = 6;
        let result = verified_path(
            0,
            MAX_DEPTH,
            |_| Ok(false),
            |node| Ok(*node + 1),
            || state.step_at(start + WALK_BUDGET),
        );
        assert_eq!(result, Err("walk_budget"));
        assert_eq!(state.header_nodes, 3, "already recorded evidence survives");
    }

    #[test]
    fn the_new_48_pixel_gap_is_diagnosed_without_a_fixed_52_pixel_assumption() {
        let frame = RectI {
            left: 222,
            top: 352,
            right: 2126,
            bottom: 1495,
        };
        let gap = RectI {
            left: 233,
            top: 394,
            right: 2108,
            bottom: 442,
        };
        assert!(header_intersects(frame, 442, Some(gap)));
        assert!(!header_intersects(frame, 394, Some(gap)));
        assert!(header_priority(frame, 442, gap) < header_priority(frame, 442, frame));
        assert!(!header_intersects(
            frame,
            442,
            Some(RectI {
                top: 442,
                bottom: 522,
                ..gap
            })
        ));
    }

    #[test]
    fn root_scoped_bulk_read_uses_cache_only_references_and_no_text_properties() {
        let automation = AutomationClient::new().unwrap();
        let cache = diagnostic_cache(&automation, ElementMode::None).unwrap();
        assert_eq!(cache.get_element_mode().unwrap(), ElementMode::None);
        assert_eq!(cache.get_tree_scope().unwrap(), TreeScope::Element);
        assert_eq!(PROPERTIES.len(), 4);
        assert!(!PROPERTIES.contains(&UIProperty::Name));
    }
}
