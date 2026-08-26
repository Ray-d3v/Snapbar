use std::{
    ffi::c_void,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use gpui::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::{
    Foundation::{HWND, POINT, RECT},
    Graphics::Dwm::{
        DWMWA_BORDER_COLOR, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
        DwmSetWindowAttribute,
    },
    UI::WindowsAndMessaging::{
        ClientToScreen, GWL_EXSTYLE, GetClientRect, GetCursorPos, GetWindowLongW, GetWindowRect,
        HWND_TOPMOST, IsIconic, IsWindow, IsWindowVisible, SW_HIDE, SW_SHOWNOACTIVATE,
        SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER,
        SWP_SHOWWINDOW, SetWindowDisplayAffinity, SetWindowLongW, SetWindowPos, ShowWindow,
        WDA_EXCLUDEFROMCAPTURE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    },
};

const FOLLOW_INTERVAL: Duration = Duration::from_millis(100);
const IDLE_INTERVAL: Duration = Duration::from_millis(500);
const TARGET_TOP_INSET: i32 = 12;
const DWMWA_COLOR_NONE: u32 = 0xffff_fffe;
const LOGICAL_WINDOW_WIDTH: i32 = 286;
const LOGICAL_WINDOW_HEIGHT: i32 = 246;
const RGN_OR: i32 = 2;

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
    fn CombineRgn(
        destination: *mut c_void,
        source1: *mut c_void,
        source2: *mut c_void,
        mode: i32,
    ) -> i32;
    fn DeleteObject(object: *mut c_void) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SetWindowRgn(hwnd: *mut c_void, region: *mut c_void, redraw: i32) -> i32;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverlayPlacement {
    Hidden,
    Visible { x: i32, y: i32 },
}

pub struct TeamsWindowFollower {
    target_id: Arc<AtomicU32>,
    menu_open: Arc<AtomicBool>,
    overlay_hwnd: isize,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl TeamsWindowFollower {
    pub fn start(window: &Window) -> Option<Self> {
        let overlay_hwnd = window_hwnd(window)?;
        configure_overlay_window(overlay_hwnd);
        apply_window_region(overlay_hwnd, false);
        unsafe {
            let _ = ShowWindow(overlay_hwnd, SW_HIDE);
        }

        let target_id = Arc::new(AtomicU32::new(0));
        let menu_open = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_target_id = Arc::clone(&target_id);
        let thread_menu_open = Arc::clone(&menu_open);
        let thread_stop = Arc::clone(&stop);
        let overlay_value = overlay_hwnd.0 as isize;
        let worker = thread::Builder::new()
            .name("snapbar-window-follow".to_string())
            .spawn(move || {
                let overlay_hwnd = HWND(overlay_value as *mut c_void);
                unsafe {
                    let _ = SetWindowDisplayAffinity(overlay_hwnd, WDA_EXCLUDEFROMCAPTURE);
                }

                let mut previous = None;
                let mut previous_client_size = None;
                while !thread_stop.load(Ordering::Acquire) {
                    let client_size = client_size(overlay_hwnd);
                    if client_size != previous_client_size {
                        apply_window_region(overlay_hwnd, thread_menu_open.load(Ordering::Acquire));
                        previous_client_size = client_size;
                    }

                    let target_id = thread_target_id.load(Ordering::Acquire);
                    let placement = if target_id == 0 {
                        OverlayPlacement::Hidden
                    } else {
                        let target_hwnd = HWND(target_id as usize as *mut c_void);
                        desired_placement(overlay_hwnd, target_hwnd)
                    };

                    if previous != Some(placement) {
                        apply_placement(overlay_hwnd, placement);
                        previous = Some(placement);
                    }

                    thread::sleep(if target_id == 0 {
                        IDLE_INTERVAL
                    } else {
                        FOLLOW_INTERVAL
                    });
                }
                apply_placement(overlay_hwnd, OverlayPlacement::Hidden);
            })
            .ok()?;

        Some(Self {
            target_id,
            menu_open,
            overlay_hwnd: overlay_value,
            stop,
            worker: Some(worker),
        })
    }

    pub fn set_target(&self, target_id: Option<u32>) {
        self.target_id
            .store(target_id.unwrap_or_default(), Ordering::Release);
    }

    pub fn set_menu_open(&self, open: bool) {
        self.menu_open.store(open, Ordering::Release);
        apply_window_region(HWND(self.overlay_hwnd as *mut c_void), open);
    }

    pub fn cursor_over_surface(&self) -> bool {
        cursor_over_visible_surface(
            HWND(self.overlay_hwnd as *mut c_void),
            self.menu_open.load(Ordering::Acquire),
        )
    }
}

impl Drop for TeamsWindowFollower {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
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
        ClientToScreen(hwnd, &mut client_origin).ok()?;
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

fn scale_logical(value: i32, actual: i32, logical: i32) -> i32 {
    ((i64::from(value) * i64::from(actual)) / i64::from(logical)) as i32
}

fn apply_window_region(hwnd: HWND, menu_open: bool) {
    let Some(metrics) = window_metrics(hwnd) else {
        return;
    };

    // SetWindowRgn is window-relative, while the GPUI layout is client-relative.
    let client_left = metrics.client_screen_left - metrics.window_left;
    let client_top = metrics.client_screen_top - metrics.window_top;
    let right = client_left + metrics.client_width + 1;

    // Include the complete 68 px control-bar surface instead of clipping to the inner 60 px pill.
    // This preserves button bottoms, antialiasing, and the shadow at every DPI.
    let bar_height = scale_logical(68, metrics.client_height, LOGICAL_WINDOW_HEIGHT)
        .clamp(1, metrics.client_height);
    let bar_bottom = client_top + bar_height + 1;
    let bar_region = unsafe {
        CreateRoundRectRgn(
            client_left,
            client_top,
            right,
            bar_bottom,
            bar_height,
            bar_height,
        )
    };
    if bar_region.is_null() {
        return;
    }

    if menu_open {
        // Overlap by 4 logical pixels so there is no native dead strip between bar and menu.
        let menu_top = client_top
            + scale_logical(64, metrics.client_height, LOGICAL_WINDOW_HEIGHT)
                .clamp(0, metrics.client_height);
        let menu_bottom = client_top + metrics.client_height + 1;
        let menu_radius = scale_logical(40, metrics.client_height, LOGICAL_WINDOW_HEIGHT).max(1);
        let menu_region = unsafe {
            CreateRoundRectRgn(
                client_left,
                menu_top,
                right,
                menu_bottom,
                menu_radius,
                menu_radius,
            )
        };
        if !menu_region.is_null() {
            let combined = unsafe { CombineRgn(bar_region, bar_region, menu_region, RGN_OR) };
            unsafe {
                let _ = DeleteObject(menu_region);
            }
            if combined == 0 {
                unsafe {
                    let _ = DeleteObject(bar_region);
                }
                return;
            }
        }
    }

    let applied = unsafe { SetWindowRgn(hwnd.0, bar_region, 1) };
    if applied == 0 {
        unsafe {
            let _ = DeleteObject(bar_region);
        }
    }
}

fn cursor_over_visible_surface(hwnd: HWND, menu_open: bool) -> bool {
    let Some(metrics) = window_metrics(hwnd) else {
        return false;
    };
    let mut cursor = POINT::default();
    if unsafe { GetCursorPos(&mut cursor) }.is_err() {
        return false;
    }

    let local_x = cursor.x - metrics.client_screen_left;
    let local_y = cursor.y - metrics.client_screen_top;
    if local_x < 0
        || local_y < 0
        || local_x >= metrics.client_width
        || local_y >= metrics.client_height
    {
        return false;
    }

    let bar_bottom = scale_logical(68, metrics.client_height, LOGICAL_WINDOW_HEIGHT)
        .clamp(1, metrics.client_height);
    if local_y < bar_bottom {
        return true;
    }

    let menu_top = scale_logical(64, metrics.client_height, LOGICAL_WINDOW_HEIGHT)
        .clamp(0, metrics.client_height);
    menu_open && local_y >= menu_top
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
    if target_width <= 0 || overlay_width <= 0 {
        return OverlayPlacement::Hidden;
    }

    OverlayPlacement::Visible {
        x: target_rect.left + (target_width - overlay_width) / 2,
        y: target_rect.top + TARGET_TOP_INSET,
    }
}

fn apply_placement(overlay_hwnd: HWND, placement: OverlayPlacement) {
    unsafe {
        match placement {
            OverlayPlacement::Hidden => {
                let _ = ShowWindow(overlay_hwnd, SW_HIDE);
            }
            OverlayPlacement::Visible { x, y } => {
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
