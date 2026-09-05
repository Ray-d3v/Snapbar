use gpui::{Bounds, Pixels, Window, fill, point, px, rgb, size};

use super::{
    OverlayPresentation, RectI, TitlebarMaterial, WINDOW_HEIGHT, WindowMetrics, WindowRegion,
    WindowRegionShape, encode_disclosure_progress, island_row_inset, scale_logical,
    window_region_for_attachment,
};

// These rectangles are the inner edge of the very same physical scanlines used
// by SetWindowRgn. No second curve, tessellation, or per-frame buffer is needed.
fn for_each_separator_strip(
    region: WindowRegion,
    thickness: i32,
    offset: u8,
    mut paint: impl FnMut(RectI),
) {
    let WindowRegionShape::Island {
        shoulder_start,
        shoulder_depth,
        shoulder_inset,
        bottom_radius,
    } = region.shape
    else {
        return;
    };
    let width = region.right - region.left;
    let height = region.bottom - region.top;
    let thickness = thickness.max(1);
    let inset_at = |row| {
        island_row_inset(
            row,
            width,
            height,
            shoulder_start,
            shoulder_depth,
            shoulder_inset,
            bottom_radius,
        )
    };
    // DWM's nominal caption bottom can precede Teams' visible separator. Above
    // that sampled row the drop and the underlying caption are the same color.
    // Leave that part unstroked so the line joins the real Teams edge precisely.
    for row in shoulder_start + i32::from(offset)..height {
        let inset = inset_at(row);
        let left = region.left + inset;
        let right = region.right - inset;
        let top = region.top + row;
        if row + thickness >= height {
            paint(RectI {
                left,
                top,
                right,
                bottom: top + 1,
            });
        } else {
            // Looking one stroke below also covers the horizontal tangent at
            // each shoulder, joining the original Teams separator at both ends.
            let inner_inset = (inset + thickness).max(inset_at(row + thickness));
            let inner_left = (region.left + inner_inset).min(right);
            let inner_right = (region.right - inner_inset).max(inner_left);
            paint(RectI {
                left,
                top,
                right: inner_left,
                bottom: top + 1,
            });
            paint(RectI {
                left: inner_right,
                top,
                right,
                bottom: top + 1,
            });
        }
    }
}

pub(crate) fn paint_island_drop(
    bounds: Bounds<Pixels>,
    progress: f32,
    material: TitlebarMaterial,
    window: &mut Window,
) {
    let scale = window.scale_factor();
    let width = (f32::from(bounds.size.width) * scale).round() as i32;
    let height = (f32::from(bounds.size.height) * scale).round() as i32;
    if width <= 0 || height <= 0 {
        return;
    }
    let progress = encode_disclosure_progress(progress);
    let region = window_region_for_attachment(
        WindowMetrics {
            window_rect: RectI {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: width,
            client_height: height,
        },
        OverlayPresentation::HoverIsland,
        false,
        false,
        progress,
    );
    let WindowRegionShape::Island { shoulder_start, .. } = region.shape else {
        return;
    };
    let drop_top = region.top + shoulder_start;
    if drop_top >= region.bottom {
        return;
    }
    let to_bounds = |rect: RectI| {
        Bounds::new(
            bounds.origin + point(px(rect.left as f32 / scale), px(rect.top as f32 / scale)),
            size(
                px((rect.right - rect.left) as f32 / scale),
                px((rect.bottom - rect.top) as f32 / scale),
            ),
        )
    };
    // The native silhouette clips this opaque extension. It also masks the old
    // straight separator inside the growing island, but leaves the caption clear.
    window.paint_quad(fill(
        to_bounds(RectI {
            left: region.left,
            top: drop_top,
            right: region.right,
            bottom: region.bottom,
        }),
        rgb(material.surface),
    ));
    if material.surface != material.separator {
        let thickness = scale_logical(1.0, height, WINDOW_HEIGHT).max(1);
        for_each_separator_strip(region, thickness, material.separator_offset, |strip| {
            window.paint_quad(fill(to_bounds(strip), rgb(material.separator)));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::{DISCLOSURE_PROGRESS_LIMIT, window_region_for_attachment};
    use super::*;
    #[test]
    fn separator_stays_inside_the_native_drop_at_every_progress_and_dpi() {
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0, 3.0] {
            let width = (super::super::WINDOW_WIDTH * scale).round() as i32;
            let height = (WINDOW_HEIGHT * scale).round() as i32;
            for progress in (0..=DISCLOSURE_PROGRESS_LIMIT).step_by(3) {
                let region = window_region_for_attachment(
                    WindowMetrics {
                        window_rect: RectI {
                            left: 0,
                            top: 0,
                            right: width,
                            bottom: height,
                        },
                        client_screen_left: 0,
                        client_screen_top: 0,
                        client_width: width,
                        client_height: height,
                    },
                    OverlayPresentation::HoverIsland,
                    false,
                    false,
                    progress,
                );
                let rectangles = super::super::coalesced_region_rectangles(region);
                let mut painted = false;
                for_each_separator_strip(region, scale.round() as i32, 0, |strip| {
                    painted = true;
                    assert!(
                        strip.top
                            >= region.top
                                + scale_logical(
                                    super::super::TITLEBAR_SURFACE_HEIGHT,
                                    height,
                                    WINDOW_HEIGHT
                                )
                    );
                    assert!(strip.right > strip.left);
                    assert!(rectangles.iter().any(|rect| strip.left >= rect.left
                        && strip.right <= rect.right
                        && strip.top >= rect.top
                        && strip.bottom <= rect.bottom));
                });
                if progress == 0 {
                    assert!(!painted);
                }
            }
        }
    }

    #[test]
    fn separator_joins_both_shoulders_and_covers_the_bottom_without_a_crossbar() {
        let region = WindowRegion {
            left: 8,
            top: 1,
            right: 296,
            bottom: 50,
            shape: WindowRegionShape::Island {
                shoulder_start: 29,
                shoulder_depth: 8,
                shoulder_inset: 24,
                bottom_radius: 12,
            },
        };
        let mut strips = Vec::new();
        for_each_separator_strip(region, 1, 0, |strip| strips.push(strip));
        let top: Vec<_> = strips.iter().filter(|s| s.top == 30).collect();
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].left, region.left);
        assert_eq!(top[1].right, region.right);
        assert!(top[0].right < 152 && top[1].left > 152);
        assert!(
            strips
                .iter()
                .any(|s| s.top == 49 && s.left < 152 && s.right > 152)
        );
    }

    #[test]
    fn separator_begins_at_the_measured_teams_edge_and_waits_for_the_drop() {
        let mut region = WindowRegion {
            left: 8,
            top: 1,
            right: 296,
            bottom: 50,
            shape: WindowRegionShape::Island {
                shoulder_start: 29,
                shoulder_depth: 8,
                shoulder_inset: 24,
                bottom_radius: 12,
            },
        };
        let mut strips = Vec::new();
        for_each_separator_strip(region, 1, 1, |strip| strips.push(strip));
        assert_eq!(strips.first().unwrap().top, 31);
        assert!(strips.iter().all(|strip| strip.top >= 31));
        region.bottom = 31;
        strips.clear();
        for_each_separator_strip(region, 1, 1, |strip| strips.push(strip));
        assert!(strips.is_empty());
    }
}
