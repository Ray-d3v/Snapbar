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
use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
    UI::WindowsAndMessaging::{
        GetWindowRect, HWND_TOPMOST, IsIconic, IsWindow, SW_HIDE, SW_SHOWNOACTIVATE,
        SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_SHOWWINDOW, SetWindowDisplayAffinity,
        SetWindowPos, ShowWindow, WDA_EXCLUDEFROMCAPTURE,
    },
};

const FOLLOW_INTERVAL: Duration = Duration::from_millis(60);
const TARGET_TOP_INSET: i32 = 12;

pub struct TeamsWindowFollower {
    target_id: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl TeamsWindowFollower {
    pub fn start(window: &Window) -> Option<Self> {
        let overlay_hwnd = window_hwnd(window)?;
        let target_id = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_target_id = Arc::clone(&target_id);
        let thread_stop = Arc::clone(&stop);
        let overlay_value = overlay_hwnd.0 as isize;
        let worker = thread::Builder::new()
            .name("snapbar-window-follow".to_string())
            .spawn(move || {
                let overlay_hwnd = HWND(overlay_value as *mut c_void);
                unsafe {
                    let _ = SetWindowDisplayAffinity(overlay_hwnd, WDA_EXCLUDEFROMCAPTURE);
                }

                while !thread_stop.load(Ordering::Acquire) {
                    let target_id = thread_target_id.load(Ordering::Acquire);
                    if target_id != 0 {
                        let target_hwnd = HWND(target_id as usize as *mut c_void);
                        follow_target(overlay_hwnd, target_hwnd);
                    }
                    thread::sleep(FOLLOW_INTERVAL);
                }
            })
            .ok()?;

        Some(Self {
            target_id,
            stop,
            worker: Some(worker),
        })
    }

    pub fn set_target(&self, target_id: Option<u32>) {
        self.target_id
            .store(target_id.unwrap_or_default(), Ordering::Release);
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
    let handle = window.window_handle().ok()?.as_raw();
    match handle {
        RawWindowHandle::Win32(handle) => Some(HWND(handle.hwnd.get() as *mut c_void)),
        _ => None,
    }
}

fn follow_target(overlay_hwnd: HWND, target_hwnd: HWND) {
    unsafe {
        if !IsWindow(target_hwnd).as_bool() {
            return;
        }
        if IsIconic(target_hwnd).as_bool() {
            let _ = ShowWindow(overlay_hwnd, SW_HIDE);
            return;
        }
    }

    let Some(target_rect) = extended_frame_bounds(target_hwnd) else {
        return;
    };
    let mut overlay_rect = RECT::default();
    unsafe {
        if GetWindowRect(overlay_hwnd, &mut overlay_rect).is_err() {
            return;
        }
    }

    let target_width = target_rect.right.saturating_sub(target_rect.left);
    let overlay_width = overlay_rect.right.saturating_sub(overlay_rect.left);
    if target_width <= 0 || overlay_width <= 0 {
        return;
    }

    let x = target_rect.left + (target_width - overlay_width) / 2;
    let y = target_rect.top + TARGET_TOP_INSET;
    unsafe {
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
