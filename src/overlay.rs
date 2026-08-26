use std::{
    ffi::c_void,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{RecvTimeoutError, SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use gpui::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::{
    Foundation::{HWND, POINT, RECT},
    Graphics::Dwm::{
        DWMWA_BORDER_COLOR, DWMWA_CAPTION_BUTTON_BOUNDS, DWMWA_EXTENDED_FRAME_BOUNDS,
        DwmGetWindowAttribute, DwmSetWindowAttribute,
    },
    UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetClientRect, GetWindowLongW, GetWindowRect, HWND_TOPMOST, IsIconic,
        IsWindow, IsWindowVisible, SW_HIDE, SW_SHOWNOACTIVATE, SWP_FRAMECHANGED, SWP_NOACTIVATE,
        SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
        SetWindowDisplayAffinity, SetWindowLongW, SetWindowPos, ShowWindow, WDA_EXCLUDEFROMCAPTURE,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    },
};

const FOLLOW_INTERVAL: Duration = Duration::from_millis(100);
const IDLE_INTERVAL: Duration = Duration::from_millis(500);
const DWMWA_COLOR_NONE: u32 = 0xffff_fffe;

pub const WINDOW_WIDTH: f32 = 280.0;
pub const WINDOW_HEIGHT: f32 = 40.0;
pub const COLLAPSED_WIDTH: f32 = 92.0;
pub const COLLAPSED_HEIGHT: f32 = 30.0;
pub const EXPANDED_WIDTH: f32 = 272.0;
pub const EXPANDED_HEIGHT: f32 = 36.0;
pub const COMPACT_WIDTH: f32 = 40.0;
pub const COMPACT_HEIGHT: f32 = 36.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverlayMode {
    Collapsed,
    Expanded,
    Compact,
}

impl OverlayMode {
    const fn from_state(expanded: bool, compact: bool) -> Self {
        if compact {
            Self::Compact
        } else if expanded {
            Self::Expanded
        } else {
            Self::Collapsed
        }
    }

    const fn logical_width(self) -> f32 {
        match self {
            Self::Collapsed => COLLAPSED_WIDTH,
            Self::Expanded => EXPANDED_WIDTH,
            Self::Compact => COMPACT_WIDTH,
        }
    }

    const fn logical_height(self) -> f32 {
        match self {
            Self::Collapsed => COLLAPSED_HEIGHT,
            Self::Expanded => EXPANDED_HEIGHT,
            Self::Compact => COMPACT_HEIGHT,
        }
    }
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateRoundRectRgn(
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        width: i32,
        height: i32,
    ) -> *mut c_void;
    fn DeleteObject(object: *mut c_void) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn ClientToScreen(hwnd: *mut c_void, point: *mut POINT) -> i32;
    fn SetWindowRgn(hwnd: *mut c_void, region: *mut c_void, redraw: i32) -> i32;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverlayPlacement {
    Hidden,
    Visible { x: i32, y: i32, compact: bool },
}

pub struct TeamsWindowFollower {
    target_id: Arc<AtomicU32>,
    expanded: Arc<AtomicBool>,
    compact: Arc<AtomicBool>,
    visible: Arc<AtomicBool>,
    wake_tx: SyncSender<()>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl TeamsWindowFollower {
    pub fn start(window: &Window) -> Option<Self> {
        let overlay_hwnd = window_hwnd(window)?;
        configure_overlay_window(overlay_hwnd);
        unsafe {
            let _ = ShowWindow(overlay_hwnd, SW_HIDE);
        }

        let target_id = Arc::new(AtomicU32::new(0));
        let expanded = Arc::new(AtomicBool::new(false));
        let compact = Arc::new(AtomicBool::new(false));
        let visible = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (wake_tx, wake_rx) = sync_channel(1);
        let thread_target_id = Arc::clone(&target_id);
        let thread_expanded = Arc::clone(&expanded);
        let thread_compact = Arc::clone(&compact);
        let thread_visible = Arc::clone(&visible);
        let thread_stop = Arc::clone(&stop);
        let overlay_value = overlay_hwnd.0 as isize;
        let worker = thread::Builder::new()
            .name("snapbar-window-follow".to_string())
            .spawn(move || {
                let overlay_hwnd = HWND(overlay_value as *mut c_void);
                unsafe {
                    let _ = SetWindowDisplayAffinity(overlay_hwnd, WDA_EXCLUDEFROMCAPTURE);
                }

                let mut previous_placement = None;
                let mut previous_region_state = None;
                while !thread_stop.load(Ordering::Acquire) {
                    let target_id = thread_target_id.load(Ordering::Acquire);
                    let placement = if target_id == 0 {
                        OverlayPlacement::Hidden
                    } else {
                        let target_hwnd = HWND(target_id as usize as *mut c_void);
                        desired_placement(overlay_hwnd, target_hwnd)
                    };

                    let visible_now = matches!(placement, OverlayPlacement::Visible { .. });
                    thread_visible.store(visible_now, Ordering::Release);
                    let compact_layout =
                        matches!(placement, OverlayPlacement::Visible { compact: true, .. });
                    thread_compact.store(compact_layout, Ordering::Release);
                    let mode = OverlayMode::from_state(
                        thread_expanded.load(Ordering::Acquire),
                        compact_layout,
                    );
                    let region_state = (client_size(overlay_hwnd), mode);
                    if previous_region_state != Some(region_state) {
                        apply_window_region(overlay_hwnd, mode);
                        previous_region_state = Some(region_state);
                    }

                    if previous_placement != Some(placement) {
                        apply_placement(overlay_hwnd, placement);
                        previous_placement = Some(placement);
                    }

                    let wait = if target_id == 0 {
                        IDLE_INTERVAL
                    } else {
                        FOLLOW_INTERVAL
                    };
                    match wake_rx.recv_timeout(wait) {
                        Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                thread_visible.store(false, Ordering::Release);
                apply_placement(overlay_hwnd, OverlayPlacement::Hidden);
            })
            .ok()?;

        Some(Self {
            target_id,
            expanded,
            compact,
            visible,
            wake_tx,
            stop,
            worker: Some(worker),
        })
    }

    fn wake(&self) {
        let _ = self.wake_tx.try_send(());
    }

    pub fn set_target(&self, target_id: Option<u32>) {
        self.target_id
            .store(target_id.unwrap_or_default(), Ordering::Release);
        self.wake();
    }

    pub fn set_expanded(&self, expanded: bool) {
        self.expanded.store(expanded, Ordering::Release);
        self.wake();
    }

    pub fn is_compact(&self) -> bool {
        self.compact.load(Ordering::Acquire)
    }

    pub fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Acquire)
    }
}
impl Drop for TeamsWindowFollower {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.wake();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn window_hwnd(window: &Window) -> Option<HWND> {
    let handle = HasWindowHandle::window_handle(window).ok()?.as_raw();
    match handle {
        RawWindowHandle::Win32(handle) => Some(HWND(handle.hwnd.get() as *mut c_void)),
        _ => None,
    }
}

fn configure_overlay_window(hwnd: HWND) {
    suppress_window_border(hwnd);
    unsafe {
        let current = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let desired = current | WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0;
        let _ = SetWindowLongW(hwnd, GWL_EXSTYLE, desired as i32);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE
                | SWP_NOSIZE
                | SWP_NOZORDER
                | SWP_NOACTIVATE
                | SWP_NOOWNERZORDER
                | SWP_FRAMECHANGED,
        );
    }
}

fn suppress_window_border(hwnd: HWND) {
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &DWMWA_COLOR_NONE as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
    }
}

#[derive(Clone, Copy, Debug)]
struct WindowMetrics {
    window_left: i32,
    window_top: i32,
    client_screen_left: i32,
    client_screen_top: i32,
    client_width: i32,
    client_height: i32,
}

fn window_metrics(hwnd: HWND) -> Option<WindowMetrics> {
    let mut client_rect = RECT::default();
    let mut window_rect = RECT::default();
    let mut client_origin = POINT::default();
    unsafe {
        GetClientRect(hwnd, &mut client_rect).ok()?;
        GetWindowRect(hwnd, &mut window_rect).ok()?;
        if ClientToScreen(hwnd.0, &mut client_origin) == 0 {
            return None;
        }
    }

    let client_width = client_rect.right.saturating_sub(client_rect.left);
    let client_height = client_rect.bottom.saturating_sub(client_rect.top);
    if client_width <= 0 || client_height <= 0 {
        return None;
    }

    Some(WindowMetrics {
        window_left: window_rect.left,
        window_top: window_rect.top,
        client_screen_left: client_origin.x,
        client_screen_top: client_origin.y,
        client_width,
        client_height,
    })
}

fn client_size(hwnd: HWND) -> Option<(i32, i32)> {
    let metrics = window_metrics(hwnd)?;
    Some((metrics.client_width, metrics.client_height))
}

fn scale_logical(value: f32, actual: i32, logical: f32) -> i32 {
    ((value * actual as f32) / logical).round() as i32
}

fn surface_rect(width: i32, height: i32, mode: OverlayMode) -> RECT {
    let surface_width = scale_logical(mode.logical_width(), width, WINDOW_WIDTH).clamp(1, width);
    let surface_height =
        scale_logical(mode.logical_height(), height, WINDOW_HEIGHT).clamp(1, height);
    let surface_left = (width - surface_width) / 2;
    let surface_top = (height - surface_height) / 2;

    RECT {
        left: surface_left,
        top: surface_top,
        right: surface_left + surface_width,
        bottom: surface_top + surface_height,
    }
}

fn apply_window_region(hwnd: HWND, mode: OverlayMode) {
    let Some(metrics) = window_metrics(hwnd) else {
        return;
    };

    let client_left = metrics.client_screen_left - metrics.window_left;
    let client_top = metrics.client_screen_top - metrics.window_top;
    let surface = surface_rect(metrics.client_width, metrics.client_height, mode);
    let height = surface.bottom.saturating_sub(surface.top).max(1);
    let region = unsafe {
        CreateRoundRectRgn(
            client_left + surface.left,
            client_top + surface.top,
            client_left + surface.right + 1,
            client_top + surface.bottom + 1,
            height,
            height,
        )
    };
    if region.is_null() {
        return;
    }

    let applied = unsafe { SetWindowRgn(hwnd.0, region, 1) };
    if applied == 0 {
        unsafe {
            let _ = DeleteObject(region);
        }
    }
}

fn desired_placement(overlay_hwnd: HWND, target_hwnd: HWND) -> OverlayPlacement {
    unsafe {
        if !IsWindow(Some(target_hwnd)).as_bool()
            || !IsWindowVisible(target_hwnd).as_bool()
            || IsIconic(target_hwnd).as_bool()
        {
            return OverlayPlacement::Hidden;
        }
    }

    let Some(target_rect) = extended_frame_bounds(target_hwnd) else {
        return OverlayPlacement::Hidden;
    };
    let mut overlay_rect = RECT::default();
    unsafe {
        if GetWindowRect(overlay_hwnd, &mut overlay_rect).is_err() {
            return OverlayPlacement::Hidden;
        }
    }

    let target_width = target_rect.right.saturating_sub(target_rect.left);
    let overlay_width = overlay_rect.right.saturating_sub(overlay_rect.left);
    let overlay_height = overlay_rect.bottom.saturating_sub(overlay_rect.top);
    if target_width <= 0 || overlay_width <= 0 || overlay_height <= 0 {
        return OverlayPlacement::Hidden;
    }

    let Some(overlay_metrics) = window_metrics(overlay_hwnd) else {
        return OverlayPlacement::Hidden;
    };
    let collapsed_surface = surface_rect(
        overlay_metrics.client_width,
        overlay_metrics.client_height,
        OverlayMode::Collapsed,
    );
    let expanded_surface = surface_rect(
        overlay_metrics.client_width,
        overlay_metrics.client_height,
        OverlayMode::Expanded,
    );
    let visible_top_offset = overlay_metrics
        .client_screen_top
        .saturating_sub(overlay_metrics.window_top)
        .saturating_add(collapsed_surface.top);
    let visible_height = collapsed_surface
        .bottom
        .saturating_sub(collapsed_surface.top);
    let expanded_width = expanded_surface.right.saturating_sub(expanded_surface.left);
    if visible_height <= 0 || expanded_width <= 0 {
        return OverlayPlacement::Hidden;
    }

    let caption = caption_button_bounds(target_hwnd);
    let caption_height = caption
        .map(|rect| rect.bottom.saturating_sub(rect.top))
        .filter(|height| *height >= 24 && *height <= 96)
        .unwrap_or(visible_height + 4);
    let caption_top = caption
        .map(|rect| rect.top)
        .filter(|top| *top >= 0 && *top <= 32)
        .unwrap_or_default();
    let caption_left = caption
        .map(|rect| rect.left)
        .filter(|left| *left > target_width / 2 && *left < target_width)
        .unwrap_or(target_width * 7 / 8);

    let safe_left = target_width / 6;
    let safe_right = caption_left.saturating_sub(8).max(target_width / 2);
    let available_width = safe_right.saturating_sub(safe_left);
    let compact = available_width < expanded_width;
    let window_center = target_width / 2;
    let safe_center = if compact {
        window_center
    } else {
        let minimum_center = safe_left.saturating_add(expanded_width / 2);
        let maximum_center = safe_right.saturating_sub(expanded_width / 2);
        window_center.clamp(minimum_center, maximum_center.max(minimum_center))
    };

    let min_x = target_rect.left;
    let max_x = target_rect.right.saturating_sub(overlay_width).max(min_x);
    let x = (target_rect.left + safe_center - overlay_width / 2).clamp(min_x, max_x);
    let visible_y = target_rect.top + caption_top + (caption_height - visible_height).max(0) / 2;
    let y = visible_y.saturating_sub(visible_top_offset);

    OverlayPlacement::Visible { x, y, compact }
}

fn apply_placement(overlay_hwnd: HWND, placement: OverlayPlacement) {
    unsafe {
        match placement {
            OverlayPlacement::Hidden => {
                let _ = ShowWindow(overlay_hwnd, SW_HIDE);
            }
            OverlayPlacement::Visible { x, y, .. } => {
                let _ = SetWindowPos(
                    overlay_hwnd,
                    Some(HWND_TOPMOST),
                    x,
                    y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
                );
                let _ = ShowWindow(overlay_hwnd, SW_SHOWNOACTIVATE);
            }
        }
    }
}

fn caption_button_bounds(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_BUTTON_BOUNDS,
            &mut rect as *mut RECT as *mut c_void,
            std::mem::size_of::<RECT>() as u32,
        )
        .ok()?;
    }
    (rect.right > rect.left && rect.bottom > rect.top).then_some(rect)
}

fn extended_frame_bounds(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut c_void,
            std::mem::size_of::<RECT>() as u32,
        )
        .ok()?;
    }
    (rect.right > rect.left && rect.bottom > rect.top).then_some(rect)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect_tuple(rect: RECT) -> (i32, i32, i32, i32) {
        (rect.left, rect.top, rect.right, rect.bottom)
    }

    #[test]
    fn compact_mode_always_wins() {
        assert_eq!(
            OverlayMode::from_state(false, false),
            OverlayMode::Collapsed
        );
        assert_eq!(OverlayMode::from_state(true, false), OverlayMode::Expanded);
        assert_eq!(OverlayMode::from_state(false, true), OverlayMode::Compact);
        assert_eq!(OverlayMode::from_state(true, true), OverlayMode::Compact);
    }

    #[test]
    fn region_matches_each_visible_surface() {
        assert_eq!(
            rect_tuple(surface_rect(280, 40, OverlayMode::Collapsed)),
            (94, 5, 186, 35)
        );
        assert_eq!(
            rect_tuple(surface_rect(280, 40, OverlayMode::Expanded)),
            (4, 2, 276, 38)
        );
        assert_eq!(
            rect_tuple(surface_rect(280, 40, OverlayMode::Compact)),
            (120, 2, 160, 38)
        );
    }

    #[test]
    fn scaled_regions_stay_centered_and_bounded() {
        for mode in [
            OverlayMode::Collapsed,
            OverlayMode::Expanded,
            OverlayMode::Compact,
        ] {
            let rect = surface_rect(350, 50, mode);
            assert!(((rect.left + rect.right) - 350).abs() <= 1);
            assert!(((rect.top + rect.bottom) - 50).abs() <= 1);
            assert!(rect.left >= 0 && rect.top >= 0);
            assert!(rect.right <= 350 && rect.bottom <= 50);
        }
    }
}
