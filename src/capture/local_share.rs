use std::{ffi::c_void, path::Path};

use anyhow::{Context as _, Result};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HWND, LPARAM, RECT},
        Graphics::{
            Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
            Gdi::{EnumDisplayMonitors, HDC, HMONITOR},
        },
        System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GWL_EXSTYLE, GetClassNameW, GetWindowDisplayAffinity, GetWindowLongW,
            GetWindowRect, GetWindowThreadProcessId, IsWindowVisible, WDA_EXCLUDEFROMCAPTURE,
            WS_EX_TOPMOST,
        },
    },
    core::{BOOL, PWSTR},
};

use super::ScreenRect;

const TEAMS_SCREEN_BORDER_CLASS: &str = "ScreenBorderWindow";
const TEAMS_EXECUTABLE_NAME: &str = "ms-teams.exe";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalMonitorCaptureTarget {
    pub monitor_handle: isize,
    pub screen_rect: ScreenRect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MonitorCandidate {
    handle: isize,
    screen_rect: ScreenRect,
}

#[derive(Default)]
struct BorderEnumeration {
    screen_rects: Vec<ScreenRect>,
}

#[derive(Default)]
struct MonitorEnumeration {
    monitors: Vec<MonitorCandidate>,
}

pub(crate) fn detect_local_monitor_target() -> Result<Option<LocalMonitorCaptureTarget>> {
    let borders = enumerate_verified_teams_borders()?;
    let monitors = enumerate_monitors()?;
    Ok(select_monitor_target(&borders, &monitors))
}

pub(crate) fn validate_local_monitor_target(expected: &LocalMonitorCaptureTarget) -> Result<bool> {
    Ok(detect_local_monitor_target()?.as_ref() == Some(expected))
}

fn enumerate_verified_teams_borders() -> Result<Vec<ScreenRect>> {
    let mut state = BorderEnumeration::default();
    unsafe {
        EnumWindows(
            Some(collect_teams_screen_border),
            LPARAM((&mut state as *mut BorderEnumeration) as isize),
        )
    }
    .context("Teamsの共有対象枠を列挙できませんでした")?;
    Ok(state.screen_rects)
}

unsafe extern "system" fn collect_teams_screen_border(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = unsafe { &mut *(lparam.0 as *mut BorderEnumeration) };
    if !unsafe { IsWindowVisible(hwnd) }.as_bool()
        || unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST.0 == 0
    {
        return BOOL::from(true);
    }

    let mut class_buffer = [0_u16; 128];
    let class_len = unsafe { GetClassNameW(hwnd, &mut class_buffer) }.max(0) as usize;
    let class_name = String::from_utf16_lossy(&class_buffer[..class_len]);
    if !class_name.eq_ignore_ascii_case(TEAMS_SCREEN_BORDER_CLASS) {
        return BOOL::from(true);
    }

    let mut display_affinity = 0_u32;
    if unsafe { GetWindowDisplayAffinity(hwnd, &mut display_affinity) }.is_err()
        || display_affinity != WDA_EXCLUDEFROMCAPTURE.0
    {
        return BOOL::from(true);
    }

    let mut cloaked = 0_u32;
    if unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast::<c_void>(),
            size_of::<u32>() as u32,
        )
    }
    .is_err()
        || cloaked != 0
    {
        return BOOL::from(true);
    }

    let mut process_id = 0_u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    if !process_is_teams(process_id) {
        return BOOL::from(true);
    }

    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok()
        && let Some(screen_rect) = screen_rect_from_rect(rect)
    {
        state.screen_rects.push(screen_rect);
    }
    BOOL::from(true)
}

fn process_is_teams(process_id: u32) -> bool {
    let Ok(process) =
        (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) })
    else {
        return false;
    };

    let mut buffer = vec![0_u16; 1_024];
    let mut length = buffer.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    };
    unsafe {
        let _ = CloseHandle(process);
    }
    if result.is_err() {
        return false;
    }

    let path = String::from_utf16_lossy(&buffer[..length as usize]);
    Path::new(&path).file_name().is_some_and(|name| {
        name.to_string_lossy()
            .eq_ignore_ascii_case(TEAMS_EXECUTABLE_NAME)
    })
}

fn enumerate_monitors() -> Result<Vec<MonitorCandidate>> {
    let mut state = MonitorEnumeration::default();
    let succeeded = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM((&mut state as *mut MonitorEnumeration) as isize),
        )
    };
    if !succeeded.as_bool() {
        return Err(windows::core::Error::from_thread())
            .context("Windowsのモニター一覧を取得できませんでした");
    }
    Ok(state.monitors)
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _: HDC,
    rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let state = unsafe { &mut *(lparam.0 as *mut MonitorEnumeration) };
    let Some(rect) = (unsafe { rect.as_ref() }).copied() else {
        return BOOL::from(true);
    };
    if let Some(screen_rect) = screen_rect_from_rect(rect) {
        state.monitors.push(MonitorCandidate {
            handle: monitor.0 as isize,
            screen_rect,
        });
    }
    BOOL::from(true)
}

fn screen_rect_from_rect(rect: RECT) -> Option<ScreenRect> {
    let width = rect.right.checked_sub(rect.left)?;
    let height = rect.bottom.checked_sub(rect.top)?;
    Some(ScreenRect {
        x: rect.left,
        y: rect.top,
        width: u32::try_from(width).ok().filter(|width| *width > 0)?,
        height: u32::try_from(height).ok().filter(|height| *height > 0)?,
    })
}

fn select_monitor_target(
    borders: &[ScreenRect],
    monitors: &[MonitorCandidate],
) -> Option<LocalMonitorCaptureTarget> {
    let [border] = borders else {
        return None;
    };
    let mut matches = monitors
        .iter()
        .filter(|monitor| monitor.screen_rect == *border);
    let monitor = *matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(LocalMonitorCaptureTarget {
        monitor_handle: monitor.handle,
        screen_rect: monitor.screen_rect,
    })
}

#[cfg(test)]
mod tests {
    use super::{MonitorCandidate, ScreenRect, select_monitor_target};

    fn rect(x: i32, y: i32, width: u32, height: u32) -> ScreenRect {
        ScreenRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn exact_unique_monitor_border_is_authoritative() {
        let target = select_monitor_target(
            &[rect(-1920, 0, 1920, 1080)],
            &[
                MonitorCandidate {
                    handle: 11,
                    screen_rect: rect(0, 0, 2560, 1440),
                },
                MonitorCandidate {
                    handle: 12,
                    screen_rect: rect(-1920, 0, 1920, 1080),
                },
            ],
        )
        .expect("the exact monitor should be selected");

        assert_eq!(target.monitor_handle, 12);
        assert_eq!(target.screen_rect, rect(-1920, 0, 1920, 1080));
    }

    #[test]
    fn multiple_or_non_monitor_borders_fail_closed() {
        let monitor = MonitorCandidate {
            handle: 12,
            screen_rect: rect(0, 0, 1920, 1080),
        };

        assert!(select_monitor_target(&[], &[monitor]).is_none());
        assert!(
            select_monitor_target(
                &[rect(0, 0, 1920, 1080), rect(0, 0, 1920, 1080)],
                &[monitor],
            )
            .is_none()
        );
        assert!(select_monitor_target(&[rect(120, 80, 1280, 720)], &[monitor]).is_none());
    }
}
