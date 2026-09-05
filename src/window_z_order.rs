use std::ffi::c_void;

use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        GW_HWNDPREV, GWL_EXSTYLE, GetWindow, GetWindowLongW, HWND_NOTOPMOST, HWND_TOPMOST,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SetWindowPos, WS_EX_TOPMOST,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverlayZOrderAnchor {
    Topmost,
    NotTopmost,
    After(isize),
}

fn choose_overlay_z_order_anchor(
    target_topmost: bool,
    overlay_topmost: bool,
    overlay: isize,
    window_above_target: Option<(isize, bool)>,
) -> Option<OverlayZOrderAnchor> {
    match window_above_target {
        Some((above, _)) if above == overlay && target_topmost == overlay_topmost => None,
        Some((above, _)) if above == overlay => Some(if target_topmost {
            OverlayZOrderAnchor::Topmost
        } else {
            OverlayZOrderAnchor::NotTopmost
        }),
        Some((_, above_topmost)) if above_topmost != target_topmost => Some(if target_topmost {
            OverlayZOrderAnchor::Topmost
        } else {
            OverlayZOrderAnchor::NotTopmost
        }),
        Some((above, _)) => Some(OverlayZOrderAnchor::After(above)),
        None => Some(if target_topmost {
            OverlayZOrderAnchor::Topmost
        } else {
            OverlayZOrderAnchor::NotTopmost
        }),
    }
}

fn window_is_topmost(hwnd: HWND) -> bool {
    unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0 }
}

pub(crate) fn sync_window_above_target(
    overlay_hwnd: HWND,
    target_hwnd: HWND,
) -> windows::core::Result<()> {
    let target_topmost = window_is_topmost(target_hwnd);
    let overlay_topmost = window_is_topmost(overlay_hwnd);
    let window_above_target = unsafe { GetWindow(target_hwnd, GW_HWNDPREV).ok() }
        .map(|above| (above.0 as isize, window_is_topmost(above)));
    let Some(anchor) = choose_overlay_z_order_anchor(
        target_topmost,
        overlay_topmost,
        overlay_hwnd.0 as isize,
        window_above_target,
    ) else {
        return Ok(());
    };
    let insert_after = match anchor {
        OverlayZOrderAnchor::Topmost => HWND_TOPMOST,
        OverlayZOrderAnchor::NotTopmost => HWND_NOTOPMOST,
        OverlayZOrderAnchor::After(window) => HWND(window as *mut c_void),
    };

    unsafe {
        SetWindowPos(
            overlay_hwnd,
            Some(insert_after),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{OverlayZOrderAnchor, choose_overlay_z_order_anchor};

    #[test]
    fn z_order_is_unchanged_when_overlay_is_already_directly_above_teams() {
        assert_eq!(
            choose_overlay_z_order_anchor(false, false, 20, Some((20, false))),
            None
        );
        assert_eq!(
            choose_overlay_z_order_anchor(true, true, 20, Some((20, true))),
            None
        );
    }

    #[test]
    fn z_order_drops_a_topmost_overlay_into_the_normal_teams_band() {
        assert_eq!(
            choose_overlay_z_order_anchor(false, true, 20, Some((20, true))),
            Some(OverlayZOrderAnchor::NotTopmost)
        );
        assert_eq!(
            choose_overlay_z_order_anchor(false, true, 20, Some((10, true))),
            Some(OverlayZOrderAnchor::NotTopmost)
        );
    }

    #[test]
    fn z_order_places_overlay_after_an_unrelated_window_above_teams() {
        assert_eq!(
            choose_overlay_z_order_anchor(false, false, 20, Some((10, false))),
            Some(OverlayZOrderAnchor::After(10))
        );
    }

    #[test]
    fn z_order_can_follow_a_topmost_teams_window_without_becoming_globally_frontmost() {
        assert_eq!(
            choose_overlay_z_order_anchor(true, false, 20, None),
            Some(OverlayZOrderAnchor::Topmost)
        );
        assert_eq!(
            choose_overlay_z_order_anchor(true, true, 20, Some((10, true))),
            Some(OverlayZOrderAnchor::After(10))
        );
    }
}
