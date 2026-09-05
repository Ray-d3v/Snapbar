use std::{
    ffi::c_void,
    sync::{Arc, Condvar, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{COLORREF, HINSTANCE, HWND, RECT},
        Graphics::{
            Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
            Gdi::UpdateWindow,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, HWND_TOPMOST, IsIconic, IsWindow, IsWindowVisible,
            LWA_ALPHA, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SWP_SHOWWINDOW,
            SetLayeredWindowAttributes, SetWindowDisplayAffinity, SetWindowPos,
            WINDOW_DISPLAY_AFFINITY, WINDOW_STYLE, WS_EX_LAYERED, WS_EX_NOACTIVATE,
            WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
        },
    },
    core::w,
};

use super::{ScreenRect, content_detector::PixelRect};
use crate::window_z_order::sync_window_above_target;

const SS_WHITERECT_STYLE: WINDOW_STYLE = WINDOW_STYLE(0x0000_0006);
const FLASH_ALPHAS: [u8; 8] = [210, 210, 176, 136, 94, 56, 24, 0];
const FLASH_STEP: Duration = Duration::from_millis(28);
const SUSPEND_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Copy)]
struct FlashRequest {
    rect: ScreenRect,
    target_window_id: Option<u32>,
    display_affinity: WINDOW_DISPLAY_AFFINITY,
}

struct FlashState {
    pending: Option<FlashRequest>,
    suspended: bool,
    suspend_ack: bool,
}

struct FlashCoordinator {
    state: Mutex<FlashState>,
    wake: Condvar,
}

static FLASH_COORDINATOR: OnceLock<Option<Arc<FlashCoordinator>>> = OnceLock::new();

pub struct FlashSuspension {
    coordinator: Arc<FlashCoordinator>,
}

impl Drop for FlashSuspension {
    fn drop(&mut self) {
        if let Ok(mut state) = self.coordinator.state.lock() {
            state.suspended = false;
            state.suspend_ack = false;
            self.coordinator.wake.notify_all();
        }
    }
}

fn coordinator() -> Option<Arc<FlashCoordinator>> {
    FLASH_COORDINATOR
        .get_or_init(|| {
            let coordinator = Arc::new(FlashCoordinator {
                state: Mutex::new(FlashState {
                    pending: None,
                    suspended: false,
                    suspend_ack: false,
                }),
                wake: Condvar::new(),
            });
            let worker_coordinator = Arc::clone(&coordinator);
            thread::Builder::new()
                .name("snapbar-flash".to_string())
                .spawn(move || flash_worker(worker_coordinator))
                .ok()?;
            Some(coordinator)
        })
        .as_ref()
        .map(Arc::clone)
}

pub fn show_capture_flash(
    rect: ScreenRect,
    target_window_id: Option<u32>,
    display_affinity: WINDOW_DISPLAY_AFFINITY,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let Some(coordinator) = coordinator() else {
        return;
    };
    if let Ok(mut state) = coordinator.state.lock() {
        if !state.suspended {
            state.pending = Some(FlashRequest {
                rect,
                target_window_id,
                display_affinity,
            });
            coordinator.wake.notify_all();
        }
    }
}

pub fn suspend_capture_flash() -> anyhow::Result<FlashSuspension> {
    let coordinator =
        coordinator().ok_or_else(|| anyhow::anyhow!("フラッシュ worker を起動できませんでした"))?;
    let deadline = Instant::now() + SUSPEND_TIMEOUT;
    let mut state = coordinator
        .state
        .lock()
        .map_err(|_| anyhow::anyhow!("フラッシュ状態を取得できませんでした"))?;
    state.suspended = true;
    state.suspend_ack = false;
    state.pending = None;
    coordinator.wake.notify_all();
    while !state.suspend_ack {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            state.suspended = false;
            coordinator.wake.notify_all();
            return Err(anyhow::anyhow!(
                "フラッシュ停止の確認がタイムアウトしました"
            ));
        }
        let (next, timeout) = coordinator
            .wake
            .wait_timeout(state, remaining)
            .map_err(|_| anyhow::anyhow!("フラッシュ停止待機に失敗しました"))?;
        state = next;
        if timeout.timed_out() && !state.suspend_ack {
            state.suspended = false;
            coordinator.wake.notify_all();
            return Err(anyhow::anyhow!(
                "フラッシュ停止の確認がタイムアウトしました"
            ));
        }
    }
    drop(state);
    Ok(FlashSuspension { coordinator })
}

pub(super) fn current_screen_rect(
    target_id: u32,
    content_rect: PixelRect,
    source_width: u32,
    source_height: u32,
) -> Option<ScreenRect> {
    if source_width == 0 || source_height == 0 {
        return None;
    }

    let hwnd = HWND(target_id as usize as *mut c_void);
    let mut window_rect = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut window_rect as *mut RECT as *mut c_void,
            std::mem::size_of::<RECT>() as u32,
        )
        .ok()?;
    }
    let window_width = window_rect.right.checked_sub(window_rect.left)?;
    let window_height = window_rect.bottom.checked_sub(window_rect.top)?;
    if window_width <= 0 || window_height <= 0 {
        return None;
    }

    let left =
        window_rect.left + scale_floor(content_rect.x, window_width as u32, source_width) as i32;
    let top =
        window_rect.top + scale_floor(content_rect.y, window_height as u32, source_height) as i32;
    let right = window_rect.left
        + scale_ceil(
            content_rect.x.saturating_add(content_rect.width),
            window_width as u32,
            source_width,
        ) as i32;
    let bottom = window_rect.top
        + scale_ceil(
            content_rect.y.saturating_add(content_rect.height),
            window_height as u32,
            source_height,
        ) as i32;

    (right > left && bottom > top).then_some(ScreenRect {
        x: left,
        y: top,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

fn flash_worker(coordinator: Arc<FlashCoordinator>) {
    run_flash_worker(coordinator, create_flash_surface);
}

fn run_flash_worker<F>(coordinator: Arc<FlashCoordinator>, mut create_surface: F)
where
    F: FnMut(FlashRequest) -> windows::core::Result<Option<FlashSurface>>,
{
    let mut active = None;
    loop {
        let request = {
            let mut state = match coordinator.state.lock() {
                Ok(state) => state,
                Err(_) => return,
            };
            loop {
                if state.suspended {
                    drop(active.take());
                    while state.suspended {
                        // Resume and suspend can both happen before this worker
                        // wakes; acknowledge the latest request on every wake.
                        state.suspend_ack = true;
                        coordinator.wake.notify_all();
                        state = match coordinator.wake.wait(state) {
                            Ok(state) => state,
                            Err(_) => return,
                        };
                    }
                    continue;
                }
                if let Some(request) = state.pending.take() {
                    break Some(request);
                }
                if active.is_some() {
                    break None;
                }
                state = match coordinator.wake.wait(state) {
                    Ok(state) => state,
                    Err(_) => return,
                };
            }
        };
        if let Some(request) = request {
            drop(active.take());
            active = create_surface(request).ok().flatten();
        }
        let Some(surface) = active.as_mut() else {
            continue;
        };
        if !surface.advance() {
            drop(active.take());
            continue;
        }
        let state = match coordinator.state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        if state.pending.is_none() && !state.suspended {
            let _ = coordinator.wake.wait_timeout(state, FLASH_STEP);
        }
    }
}

fn create_flash_surface(request: FlashRequest) -> windows::core::Result<Option<FlashSurface>> {
    let target_hwnd = request
        .target_window_id
        .map(|id| HWND(id as usize as *mut c_void));
    if !flash_target_is_visible(target_hwnd) {
        return Ok(None);
    }
    let module = unsafe { GetModuleHandleW(None)? };
    let instance = HINSTANCE(module.0);
    let ex_style = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
    let style = WS_POPUP | SS_WHITERECT_STYLE;
    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            w!("STATIC"),
            w!(""),
            style,
            request.rect.x,
            request.rect.y,
            request.rect.width.min(i32::MAX as u32) as i32,
            request.rect.height.min(i32::MAX as u32) as i32,
            None,
            None,
            Some(instance),
            None,
        )?
    };
    let window = FlashWindow(hwnd);

    unsafe {
        let _ = SetWindowDisplayAffinity(hwnd, request.display_affinity);
        SetLayeredWindowAttributes(hwnd, COLORREF(0), FLASH_ALPHAS[0], LWA_ALPHA)?;
        position_flash_window(hwnd, target_hwnd, request.rect)?;
        let _ = UpdateWindow(hwnd);
    }
    Ok(Some(FlashSurface {
        window,
        target: target_hwnd,
        next_alpha: 1,
    }))
}

struct FlashSurface {
    window: FlashWindow,
    target: Option<HWND>,
    next_alpha: usize,
}

impl FlashSurface {
    fn advance(&mut self) -> bool {
        if self.next_alpha >= FLASH_ALPHAS.len() || !flash_target_is_visible(self.target) {
            return false;
        }
        let alpha = FLASH_ALPHAS[self.next_alpha];
        self.next_alpha += 1;
        if let Some(target) = self.target {
            if sync_window_above_target(self.window.0, target).is_err() {
                return false;
            }
        }
        unsafe { SetLayeredWindowAttributes(self.window.0, COLORREF(0), alpha, LWA_ALPHA).is_ok() }
    }
}

fn position_flash_window(
    hwnd: HWND,
    target: Option<HWND>,
    rect: ScreenRect,
) -> windows::core::Result<()> {
    let mut flags = SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW;
    let insert_after = if let Some(target) = target {
        sync_window_above_target(hwnd, target)?;
        flags |= SWP_NOZORDER;
        None
    } else {
        Some(HWND_TOPMOST)
    };
    unsafe {
        SetWindowPos(
            hwnd,
            insert_after,
            rect.x,
            rect.y,
            rect.width.min(i32::MAX as u32) as i32,
            rect.height.min(i32::MAX as u32) as i32,
            flags,
        )
    }
}

struct FlashWindow(HWND);

impl Drop for FlashWindow {
    fn drop(&mut self) {
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

fn flash_target_is_visible(target: Option<HWND>) -> bool {
    target.is_none_or(|hwnd| unsafe {
        IsWindow(Some(hwnd)).as_bool()
            && IsWindowVisible(hwnd).as_bool()
            && !IsIconic(hwnd).as_bool()
    })
}

fn scale_floor(value: u32, target: u32, source: u32) -> u32 {
    ((u64::from(value) * u64::from(target)) / u64::from(source)).min(u64::from(target)) as u32
}

fn scale_ceil(value: u32, target: u32, source: u32) -> u32 {
    (u64::from(value) * u64::from(target))
        .div_ceil(u64::from(source))
        .min(u64::from(target)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        GW_HWNDPREV, GWL_EXSTYLE, GetWindow, GetWindowLongW, HWND_TOP, SWP_NOMOVE, SWP_NOSIZE,
        WS_EX_TOPMOST,
    };

    fn hidden_window() -> FlashWindow {
        FlashWindow(
            unsafe {
                CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    w!("STATIC"),
                    w!("Snapbar flash regression"),
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .unwrap(),
        )
    }

    #[test]
    fn remote_flash_stays_below_the_window_covering_teams() {
        let target = hidden_window();
        let flash = hidden_window();
        let covering_window = hidden_window();
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
        unsafe {
            SetWindowPos(covering_window.0, Some(HWND_TOP), 0, 0, 0, 0, flags).unwrap();
            SetWindowPos(target.0, Some(covering_window.0), 0, 0, 0, 0, flags).unwrap();
        }

        // Exercise the production show path with a zero-area surface so that
        // SWP_SHOWWINDOW is tested without displaying anything on the desktop.
        position_flash_window(
            flash.0,
            Some(target.0),
            ScreenRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
        )
        .unwrap();

        unsafe {
            assert_eq!(GetWindow(target.0, GW_HWNDPREV).unwrap(), flash.0);
            assert_eq!(GetWindow(flash.0, GW_HWNDPREV).unwrap(), covering_window.0);
            assert_eq!(
                GetWindowLongW(flash.0, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0,
                0
            );
        }

        // A later covering window must also remain above the flash.
        let next_window = hidden_window();
        unsafe {
            SetWindowPos(next_window.0, Some(flash.0), 0, 0, 0, 0, flags).unwrap();
        }
        sync_window_above_target(flash.0, target.0).unwrap();
        unsafe {
            assert_eq!(GetWindow(target.0, GW_HWNDPREV).unwrap(), flash.0);
            assert_eq!(GetWindow(flash.0, GW_HWNDPREV).unwrap(), next_window.0);
        }
    }

    #[test]
    fn hidden_or_closed_remote_target_has_no_flash() {
        let target = hidden_window();
        assert!(!flash_target_is_visible(Some(target.0)));
        assert!(!flash_target_is_visible(Some(HWND::default())));
        assert!(flash_target_is_visible(None));
    }

    #[test]
    fn scaling_keeps_bounds_inside_target() {
        assert_eq!(scale_floor(100, 1500, 1000), 150);
        assert_eq!(scale_ceil(101, 1500, 1000), 152);
    }

    #[test]
    fn invalid_source_size_has_no_screen_rect() {
        assert!(current_screen_rect(0, PixelRect::new(0, 0, 1, 1), 0, 1).is_none());
    }

    fn zero_area_request() -> FlashRequest {
        FlashRequest {
            rect: ScreenRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            target_window_id: None,
            display_affinity: WINDOW_DISPLAY_AFFINITY(0),
        }
    }

    fn test_coordinator() -> Arc<FlashCoordinator> {
        Arc::new(FlashCoordinator {
            state: Mutex::new(FlashState {
                pending: None,
                suspended: false,
                suspend_ack: false,
            }),
            wake: Condvar::new(),
        })
    }

    #[test]
    fn worker_releases_real_surface_after_complete_fade() {
        let coordinator = test_coordinator();
        let (created_tx, created_rx) = std::sync::mpsc::channel();
        let worker_coordinator = Arc::clone(&coordinator);
        let _worker = thread::spawn(move || {
            run_flash_worker(worker_coordinator, move |request| {
                let surface = create_flash_surface(request)?;
                if let Some(ref surface) = surface {
                    created_tx.send(surface.window.0.0 as usize).unwrap();
                }
                Ok(surface)
            });
        });
        {
            let mut state = coordinator.state.lock().unwrap();
            state.pending = Some(zero_area_request());
            coordinator.wake.notify_all();
        }
        let hwnd = created_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(unsafe { IsWindow(Some(HWND(hwnd as *mut c_void))) }.as_bool());
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline
            && unsafe { IsWindow(Some(HWND(hwnd as *mut c_void))) }.as_bool()
        {
            thread::sleep(FLASH_STEP);
        }
        assert!(!unsafe { IsWindow(Some(HWND(hwnd as *mut c_void))) }.as_bool());
    }

    #[test]
    fn worker_acknowledges_suspend_and_releases_active_real_surface() {
        let coordinator = Arc::new(FlashCoordinator {
            state: Mutex::new(FlashState {
                pending: None,
                suspended: false,
                suspend_ack: false,
            }),
            wake: Condvar::new(),
        });
        let worker_coordinator = Arc::clone(&coordinator);
        let (created_tx, created_rx) = std::sync::mpsc::channel();
        let _worker = thread::spawn(move || {
            run_flash_worker(worker_coordinator, move |request| {
                let surface = create_flash_surface(request)?;
                if let Some(ref surface) = surface {
                    created_tx.send(surface.window.0.0 as usize).unwrap();
                }
                Ok(surface)
            });
        });

        {
            let mut state = coordinator.state.lock().unwrap();
            state.pending = Some(zero_area_request());
            coordinator.wake.notify_all();
        }

        let hwnd = created_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let mut state = coordinator.state.lock().unwrap();
        assert!(state.pending.is_none());
        state.suspended = true;
        state.suspend_ack = false;
        coordinator.wake.notify_all();
        let (state_after, timeout) = coordinator
            .wake
            .wait_timeout_while(state, Duration::from_millis(500), |state| {
                !state.suspend_ack
            })
            .unwrap();
        assert!(!timeout.timed_out());
        assert!(state_after.suspend_ack);
        assert!(!unsafe { IsWindow(Some(HWND(hwnd as *mut c_void))) }.as_bool());
    }

    #[test]
    fn worker_reacknowledges_after_suspend_is_reasserted_before_wake() {
        let coordinator = test_coordinator();
        let worker_coordinator = Arc::clone(&coordinator);
        let _worker = thread::spawn(move || {
            run_flash_worker(worker_coordinator, |_| Ok(None));
        });

        let mut state = coordinator.state.lock().unwrap();
        state.suspended = true;
        state.suspend_ack = false;
        coordinator.wake.notify_all();
        let (next, timeout) = coordinator
            .wake
            .wait_timeout_while(state, Duration::from_millis(500), |state| {
                !state.suspend_ack
            })
            .unwrap();
        assert!(!timeout.timed_out());
        state = next;
        // Keep the lock across resume and the next suspension so the worker
        // can only observe the final suspended state when it wakes.
        state.suspended = false;
        state.suspend_ack = false;
        coordinator.wake.notify_all();
        state.suspended = true;
        state.suspend_ack = false;
        coordinator.wake.notify_all();
        drop(state);

        let state = coordinator.state.lock().unwrap();
        let (state, timeout) = coordinator
            .wake
            .wait_timeout_while(state, Duration::from_millis(500), |state| {
                !state.suspend_ack
            })
            .unwrap();
        assert!(!timeout.timed_out());
        assert!(state.suspend_ack);
    }
}
