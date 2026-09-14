//! A root-scoped titlebar measurement, independent of the meeting toolbar.
//! Anonymous containers are not sufficient evidence: they must contain both
//! the left MenuBar and the right more-options-header in one coherent band.
use super::RectI;
use uiautomation::{
    UIAutomation, UIElement,
    core::UICacheRequest,
    types::{ControlType, ElementMode, TreeScope, UIProperty},
};

const MAX_ELEMENTS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Container,
    Menu,
    More,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Node {
    role: Role,
    rect: RectI,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Selection {
    Unavailable,
    Invalid,
    Boundary(i32),
}

impl Selection {
    pub(super) fn or_else(self, fallback: impl FnOnce() -> Option<i32>) -> Option<i32> {
        match self {
            Self::Unavailable => fallback(),
            Self::Invalid => None,
            Self::Boundary(bottom) => Some(bottom),
        }
    }
}

#[derive(Default)]
pub(super) struct HeaderProbe {
    // All references originate in the selected HWND's FindAllBuildCache query.
    // They are never shared across workers or retained across rediscovery.
    elements: Vec<(Role, UIElement)>,
    cache: Option<UICacheRequest>,
    overflow: bool,
}

impl HeaderProbe {
    pub(super) fn clear(&mut self) {
        self.elements.clear();
        self.overflow = false;
    }

    pub(super) fn prepare(&mut self, automation: &UIAutomation) {
        if self.cache.is_none() {
            self.cache = current_cache(automation).ok();
        }
    }

    pub(super) fn add_cached(&mut self, element: &UIElement, frame: RectI, dpi: u32) {
        let Some(node) = cached_node(element) else {
            return;
        };
        if node.role == Role::Container && !container_in_frame(frame, dpi, node.rect) {
            return;
        }
        if self.elements.len() == MAX_ELEMENTS {
            self.overflow = true;
            return;
        }
        self.elements.push((node.role, element.clone()));
    }

    pub(super) fn cached(&self, frame: RectI, dpi: u32) -> Selection {
        self.read(frame, dpi, |element| Some(element.clone()))
    }

    pub(super) fn current(&self, frame: RectI, dpi: u32) -> Selection {
        self.read(frame, dpi, |element| {
            // Refresh identity, kind, visibility and rectangle together. The
            // previous cached values are never current zoom/layout evidence.
            element.build_updated_cache(self.cache.as_ref()?).ok()
        })
    }

    fn read(
        &self,
        frame: RectI,
        dpi: u32,
        mut refresh: impl FnMut(&UIElement) -> Option<UIElement>,
    ) -> Selection {
        if self.overflow {
            return Selection::Invalid;
        }
        let mut nodes = Vec::with_capacity(self.elements.len());
        for (role, element) in &self.elements {
            let Some(node) = refresh(element).and_then(|element| cached_node(&element)) else {
                return Selection::Invalid;
            };
            if node.role != *role {
                return Selection::Invalid;
            }
            nodes.push(node);
        }
        select(frame, dpi, &nodes)
    }
}

fn current_cache(automation: &UIAutomation) -> uiautomation::Result<UICacheRequest> {
    let request = automation.create_cache_request()?;
    request.set_tree_scope(TreeScope::Element)?;
    request.set_tree_filter(automation.create_true_condition()?)?;
    request.set_element_mode(ElementMode::None)?;
    for property in [
        UIProperty::AutomationId,
        UIProperty::ControlType,
        UIProperty::IsOffscreen,
        UIProperty::BoundingRectangle,
    ] {
        request.add_property(property)?;
    }
    Ok(request)
}

fn role(kind: ControlType, id: &str) -> Option<Role> {
    match (kind, id) {
        (ControlType::Button, "more-options-header") => Some(Role::More),
        (ControlType::MenuBar, "MenuBar") => Some(Role::Menu),
        (ControlType::Pane | ControlType::Group, _) => Some(Role::Container),
        _ => None,
    }
}

fn cached_node(element: &UIElement) -> Option<Node> {
    if element.is_cached_offscreen().ok()? {
        return None;
    }
    let kind = element.get_cached_control_type().ok()?;
    let id = element.get_cached_automation_id().ok()?;
    let role = role(kind, &id)?;
    let rect = element.get_cached_bounding_rectangle().ok()?;
    let rect = RectI {
        left: rect.get_left(),
        top: rect.get_top(),
        right: rect.get_right(),
        bottom: rect.get_bottom(),
    };
    positive(rect).then_some(Node { role, rect })
}

fn positive(rect: RectI) -> bool {
    rect.right > rect.left && rect.bottom > rect.top
}

fn height(rect: RectI) -> i64 {
    i64::from(rect.bottom) - i64::from(rect.top)
}

fn tolerance(dpi: u32) -> Option<i64> {
    (96..=768).contains(&dpi).then_some(i64::from(dpi.div_ceil(96)))
}

fn near(a: i32, b: i32, tolerance: i64) -> bool {
    (i64::from(a) - i64::from(b)).abs() <= tolerance
}

fn contains(outer: RectI, inner: RectI, tolerance: i64) -> bool {
    positive(outer)
        && positive(inner)
        && i64::from(inner.left) >= i64::from(outer.left) - tolerance
        && i64::from(inner.right) <= i64::from(outer.right) + tolerance
        && i64::from(inner.top) >= i64::from(outer.top) - tolerance
        && i64::from(inner.bottom) <= i64::from(outer.bottom) + tolerance
}

fn container_in_frame(frame: RectI, dpi: u32, rect: RectI) -> bool {
    let Some(tolerance) = tolerance(dpi) else {
        return false;
    };
    positive(frame)
        && positive(rect)
        && near(rect.top, frame.top, tolerance)
        && near(rect.left, frame.left, 2 * tolerance)
        && near(rect.right, frame.right, 2 * tolerance)
        && height(rect) >= i64::from(dpi) * 12 / 96
        && height(rect) * 2 < height(frame)
}

fn unique_marker(nodes: &[Node], role: Role) -> Result<Option<RectI>, ()> {
    let mut found = None;
    for node in nodes.iter().filter(|node| node.role == role) {
        if found.is_some_and(|rect| rect != node.rect) {
            return Err(());
        }
        found = Some(node.rect);
    }
    Ok(found)
}

fn select(frame: RectI, dpi: u32, nodes: &[Node]) -> Selection {
    let Some(tolerance) = tolerance(dpi) else {
        return Selection::Invalid;
    };
    if !positive(frame) || nodes.len() > MAX_ELEMENTS {
        return Selection::Invalid;
    }
    let (menu, more) = match (
        unique_marker(nodes, Role::Menu),
        unique_marker(nodes, Role::More),
    ) {
        (Ok(Some(menu)), Ok(Some(more))) => (menu, more),
        (Err(()), _) | (_, Err(())) => return Selection::Invalid,
        // Older Teams builds can still use the existing caption-row fallback.
        _ => return Selection::Unavailable,
    };
    let center = (i64::from(frame.left) + i64::from(frame.right)) / 2;
    if !contains(frame, menu, tolerance)
        || !contains(frame, more, tolerance)
        || i64::from(menu.right) >= center
        || i64::from(more.left) <= center
        || menu.top.max(more.top) >= menu.bottom.min(more.bottom)
    {
        return Selection::Invalid;
    }
    let marker_height = height(menu).max(height(more));
    let mut bottoms = Vec::new();
    for node in nodes.iter().filter(|node| node.role == Role::Container) {
        let rect = node.rect;
        if container_in_frame(frame, dpi, rect)
            && contains(rect, menu, tolerance)
            && contains(rect, more, tolerance)
            && height(rect) <= marker_height * 2
        {
            bottoms.push(rect.bottom);
        }
    }
    let Some(&bottom) = bottoms.iter().min() else {
        return Selection::Invalid;
    };
    // Pane and Group wrappers can differ by one scaled border pixel. Distinct
    // bands are ambiguous, not permission to choose an arbitrary larger one.
    if bottoms.iter().all(|value| near(*value, bottom, tolerance)) {
        Selection::Boundary(bottom)
    } else {
        Selection::Invalid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> RectI {
        RectI {
            left: 757,
            top: 171,
            right: 3447,
            bottom: 1853,
        }
    }

    fn nodes() -> Vec<Node> {
        vec![
            Node {
                role: Role::Container,
                rect: RectI {
                    left: 758,
                    top: 171,
                    right: 3446,
                    bottom: 213,
                },
            },
            Node {
                role: Role::Container,
                rect: RectI {
                    left: 758,
                    top: 171,
                    right: 3447,
                    bottom: 214,
                },
            },
            Node {
                role: Role::Menu,
                rect: RectI {
                    left: 758,
                    top: 179,
                    right: 780,
                    bottom: 201,
                },
            },
            Node {
                role: Role::More,
                rect: RectI {
                    left: 3224,
                    top: 176,
                    right: 3257,
                    bottom: 208,
                },
            },
        ]
    }

    #[test]
    fn measured_titlebar_does_not_include_a_sharing_banner() {
        let mut nodes = nodes();
        assert_eq!(select(frame(), 96, &nodes), Selection::Boundary(213));
        for gap in [48, 52, 96] {
            nodes.push(Node {
                role: Role::Container,
                rect: RectI {
                    left: 758,
                    top: 213,
                    right: 3447,
                    bottom: 213 + gap + 1,
                },
            });
            assert_eq!(select(frame(), 96, &nodes), Selection::Boundary(213));
            nodes.pop();
        }
    }

    #[test]
    fn missing_header_identity_uses_only_the_existing_fallback() {
        let nodes = nodes();
        assert_eq!(select(frame(), 96, &nodes[..2]), Selection::Unavailable);
        assert_eq!(Selection::Unavailable.or_else(|| Some(45)), Some(45));
        assert_eq!(Selection::Invalid.or_else(|| panic!("ambiguous header")), None);
    }

    #[test]
    fn unrelated_ids_and_control_types_are_not_header_markers() {
        assert_eq!(role(ControlType::Button, "more-options-header"), Some(Role::More));
        assert_eq!(role(ControlType::MenuBar, "MenuBar"), Some(Role::Menu));
        for id in ["", "more-options-header-copy", "close", "MenuBar"] {
            assert_ne!(role(ControlType::Button, id), Some(Role::More));
        }
        assert_ne!(role(ControlType::Text, "more-options-header"), Some(Role::More));
        assert_ne!(role(ControlType::Button, "MenuBar"), Some(Role::Menu));
    }

    #[test]
    fn competing_bands_or_duplicate_marker_locations_fail_closed() {
        let mut nodes = nodes();
        nodes.push(Node {
            role: Role::Container,
            rect: RectI { bottom: 230, ..nodes[0].rect },
        });
        assert_eq!(select(frame(), 96, &nodes), Selection::Invalid);
        nodes.pop();
        nodes.push(Node {
            role: Role::More,
            rect: RectI { left: 3230, ..nodes[3].rect },
        });
        assert_eq!(select(frame(), 96, &nodes), Selection::Invalid);
    }

    #[test]
    fn full_window_and_thin_border_are_not_a_titlebar() {
        let mut nodes = nodes();
        nodes[0].rect = frame();
        nodes[1].rect.bottom = 176;
        assert_eq!(select(frame(), 96, &nodes), Selection::Invalid);
    }

    #[test]
    fn header_markers_must_be_in_the_same_selected_window_band() {
        let original = nodes();
        for delta in [(0, 80), (3000, 0), (-3000, 0)] {
            let mut changed = original.clone();
            changed[3].rect = changed[3].rect.offset(delta.0, delta.1);
            assert_eq!(select(frame(), 96, &changed), Selection::Invalid);
        }
    }

    #[test]
    fn current_geometry_follows_zoom_and_negative_monitor_origins() {
        let original = nodes();
        for dpi in [96, 110, 120, 144, 168, 192, 240, 288] {
            for zoom in [0.75_f64, 1.0, 1.5, 2.0, 3.0] {
                let scale = f64::from(dpi) / 96.0;
                let map = |rect: RectI| RectI {
                    left: -5000 + ((rect.left - frame().left) as f64 * scale).round() as i32,
                    top: -2000 + ((rect.top - frame().top) as f64 * scale * zoom).round() as i32,
                    right: -5000 + ((rect.right - frame().left) as f64 * scale).round() as i32,
                    bottom: -2000 + ((rect.bottom - frame().top) as f64 * scale * zoom).round() as i32,
                };
                // The second wrapper's border is a physical UIA rounding edge,
                // not a Teams zoom-dependent padding value.
                let mut scaled: Vec<_> = original.iter().map(|node| Node {
                    role: node.role,
                    rect: map(node.rect),
                }).collect();
                scaled[1].rect.bottom = scaled[0].rect.bottom + dpi.div_ceil(96) as i32;
                assert_eq!(select(map(frame()), dpi, &scaled), Selection::Boundary(scaled[0].rect.bottom));
            }
        }
    }

    #[test]
    fn invalid_geometry_and_unbounded_candidate_sets_are_rejected() {
        let nodes = nodes();
        for dpi in [0, 95, 769, u32::MAX] {
            assert_eq!(select(frame(), dpi, &nodes), Selection::Invalid);
        }
        assert_eq!(select(RectI { right: 0, ..frame() }, 96, &nodes), Selection::Invalid);
        let many = vec![nodes[0]; MAX_ELEMENTS + 1];
        assert_eq!(select(frame(), 96, &many), Selection::Invalid);
        assert!(!container_in_frame(frame(), 96, RectI {
            left: i32::MIN, top: i32::MIN, right: i32::MAX, bottom: i32::MAX,
        }));
    }

    #[test]
    fn current_cache_requests_only_metadata_and_no_ui_text() {
        let automation = crate::automation::AutomationClient::new().unwrap();
        let request = current_cache(&automation).unwrap();
        let element = automation.get_root_element().unwrap().build_updated_cache(&request).unwrap();
        assert!(element.get_cached_control_type().is_ok());
        assert!(element.is_cached_offscreen().is_ok());
        assert!(element.get_cached_bounding_rectangle().is_ok());
        assert!(element.get_cached_name().is_err());
    }
}
