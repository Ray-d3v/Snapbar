use std::{ffi::c_void, thread, time::Duration};

use windows::{
    Win32::{
        Foundation::{COLORREF, HINSTANCE, HWND, RECT},
        Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, HWND_TOPMOST, LWA_ALPHA, SW_SHOWNOACTIVATE,
            SWP_NOACTIVATE, SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetWindowDisplayAffinity,
            SetWindowPos, ShowWindow, UpdateWindow, WDA_EXCLUDEFROMCAPTURE, WINDOW_STYLE,
            WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
        },
    },
    core::w,
};

use super::{ScreenRect, content_detector::PixelRect};

const SS_WHITERECT_STYLE: WINDOW_STYLE = WINDOW_STYLE(0x0000_0006);
const FLASH_ALPHAS: [u8; 8] = [210, 210, 176, 136, 94, 56, 24, 0];
const FLASH_STEP: Duration = Duration::from_millis(28);

pub fn show_capture_flash(rect: ScreenRect) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let _ = thread::Builder::new()
        .name("snapbar-flash".to_string())
        .spawn(move || {
            let _ = flash_window(rect);
        });
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

fn flash_window(rect: ScreenRect) -> windows::core::Result<()> {
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
            rect.x,
            rect.y,
            rect.width.min(i32::MAX as u32) as i32,
            rect.height.min(i32::MAX as u32) as i32,
            None,
            None,
            Some(instance),
            None,
        )?
    };

    unsafe {
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        SetLayeredWindowAttributes(hwnd, COLORREF(0), FLASH_ALPHAS[0], LWA_ALPHA)?;
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            rect.x,
            rect.y,
            rect.width.min(i32::MAX as u32) as i32,
            rect.height.min(i32::MAX as u32) as i32,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )?;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        UpdateWindow(hwnd)?;
    }

    for alpha in FLASH_ALPHAS.into_iter().skip(1) {
        thread::sleep(FLASH_STEP);
        unsafe {
            if SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA).is_err() {
                break;
            }
        }
    }

    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    Ok(())
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
    use super::{current_screen_rect, scale_ceil, scale_floor};
    use crate::capture::content_detector::PixelRect;

    #[test]
    fn scaling_keeps_bounds_inside_target() {
        assert_eq!(scale_floor(100, 1500, 1000), 150);
        assert_eq!(scale_ceil(101, 1500, 1000), 152);
    }

    #[test]
    fn invalid_source_size_has_no_screen_rect() {
        assert!(current_screen_rect(0, PixelRect::new(0, 0, 1, 1), 0, 1).is_none());
    }
}
