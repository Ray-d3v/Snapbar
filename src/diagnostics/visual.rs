//! Read-only, request-scoped native-window observations. These are diagnostics,
//! not capture authorization or an atomic description of a displayed frame.
use std::{
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, POINT, RECT},
        Graphics::Gdi::{ClientToScreen, GetWindowRgnBox},
        System::Threading::GetCurrentThreadId,
        UI::{
            HiDpi::GetDpiForWindow,
            WindowsAndMessaging::{
                EnumThreadWindows, GA_ROOT, GW_HWNDPREV, GWL_EXSTYLE, GetAncestor,
                GetClientRect, GetCursorPos, GetWindow, GetWindowDisplayAffinity,
                GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, IsWindow,
                IsWindowVisible, WindowFromPoint,
            },
        },
    },
    core::BOOL,
};

const MAX_WINDOWS: usize = 16;
const SNAPSHOT_BUDGET: Duration = Duration::from_millis(10);
const CONTEXT_DURATION: Duration = Duration::from_secs(2);
static GUI_THREAD: OnceLock<u32> = OnceLock::new();
static NEXT_SCOPE: AtomicU64 = AtomicU64::new(1);
static RECENT: Mutex<Option<(u64, Instant)>> = Mutex::new(None);

pub(super) fn init() {
    let _ = GUI_THREAD.set(unsafe { GetCurrentThreadId() });
}

pub(super) struct ScopeTrace {
    request: u64,
    scope: u64,
}

impl ScopeTrace {
    pub(super) fn enter(request: u64) -> Option<Self> {
        if request == 0 || !super::enabled() || GUI_THREAD.get().is_none() {
            return None;
        }
        if let Ok(mut recent) = RECENT.try_lock() {
            *recent = Some((request, Instant::now()));
        }
        let trace = Self {
            request,
            scope: NEXT_SCOPE.fetch_add(1, Ordering::Relaxed),
        };
        snapshot("scope_enter", trace.request, trace.scope);
        Some(trace)
    }

    pub(super) fn finish(self) {
        snapshot("scope_exit", self.request, self.scope);
    }
}

fn recent_at(last: Option<(u64, Instant)>, now: Instant) -> Option<u64> {
    last.filter(|(_, started)| now.saturating_duration_since(*started) < CONTEXT_DURATION)
        .map(|(request, _)| request)
}

// Advisory correlation only. A contended lock or expired context returns None;
// neither outcome may affect sampling, capture, or publication.
pub(crate) fn recent_capture() -> Option<u64> {
    let recent = *RECENT.try_lock().ok()?;
    recent_at(recent, Instant::now())
}

#[derive(Default)]
struct Handles {
    values: [isize; MAX_WINDOWS],
    count: usize,
    truncated: bool,
}

impl Handles {
    fn push(&mut self, hwnd: isize) -> bool {
        if self.count == MAX_WINDOWS {
            self.truncated = true;
            return false;
        }
        self.values[self.count] = hwnd;
        self.count += 1;
        true
    }
}

fn belongs_to_this_process(hwnd: HWND) -> bool {
    let mut process = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)) };
    process == std::process::id()
}

unsafe extern "system" fn collect(hwnd: HWND, parameter: LPARAM) -> BOOL {
    // EnumThreadWindows is synchronous; the stack value outlives every callback.
    let handles = unsafe { &mut *(parameter.0 as *mut Handles) };
    (!belongs_to_this_process(hwnd) || handles.push(hwnd.0 as isize)).into()
}

type Rect = (i32, i32, i32, i32);
fn coordinates(rect: RECT) -> Rect {
    (rect.left, rect.top, rect.right, rect.bottom)
}

#[derive(Debug, PartialEq, Eq)]
struct WindowFacts {
    visible: bool,
    dpi: u32,
    window: Option<Rect>,
    client: Option<Rect>,
    client_origin: Option<(i32, i32)>,
    region_kind: i32,
    region_window_relative: Option<Rect>,
    affinity: Option<u32>,
    affinity_error: Option<i32>,
    ex_style: u32,
    previous_window: Option<isize>,
}

fn facts(hwnd: HWND) -> Option<WindowFacts> {
    if !belongs_to_this_process(hwnd) || !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        return None;
    }
    let mut rect = RECT::default();
    let window = unsafe { GetWindowRect(hwnd, &mut rect) }
        .ok()
        .map(|_| coordinates(rect));
    let client = unsafe { GetClientRect(hwnd, &mut rect) }
        .ok()
        .map(|_| coordinates(rect));
    let mut origin = POINT::default();
    let client_origin = unsafe { ClientToScreen(hwnd, &mut origin) }
        .as_bool()
        .then_some((origin.x, origin.y));
    let region_kind = unsafe { GetWindowRgnBox(hwnd, &mut rect) }.0;
    // ERROR (0) also means no explicit region. Never interpret that as a box.
    let region_window_relative = (region_kind != 0).then_some(coordinates(rect));
    let mut affinity = 0;
    let affinity_result = unsafe { GetWindowDisplayAffinity(hwnd, &mut affinity) };
    Some(WindowFacts {
        visible: unsafe { IsWindowVisible(hwnd).as_bool() },
        dpi: unsafe { GetDpiForWindow(hwnd) },
        window,
        client,
        client_origin,
        region_kind,
        region_window_relative,
        affinity: affinity_result.is_ok().then_some(affinity),
        affinity_error: affinity_result.err().map(|error| error.code().0),
        ex_style: unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32,
        previous_window: unsafe { GetWindow(hwnd, GW_HWNDPREV) }
            .ok()
            .map(|window| window.0 as isize),
    })
}

pub(crate) fn capture_windows(phase: &str, request: u64) {
    if super::enabled() {
        snapshot(phase, request, 0);
    }
}

pub(crate) fn window_snapshot(phase: &str, request: u64, hwnd: HWND) {
    if !super::enabled() {
        return;
    }
    let _physical = crate::dpi::PhysicalPixels::enter();
    super::log(format_args!(
        "visual_window phase={phase} capture_request={request} hwnd={} facts={:?} atomic=false",
        hwnd.0 as isize,
        facts(hwnd)
    ));
}

fn snapshot(phase: &str, request: u64, scope: u64) {
    let Some(&gui_thread) = GUI_THREAD.get() else {
        return;
    };
    let started = Instant::now();
    let _physical = crate::dpi::PhysicalPixels::enter();
    let mut handles = Handles::default();
    let complete = unsafe {
        EnumThreadWindows(
            gui_thread,
            Some(collect),
            LPARAM(&mut handles as *mut Handles as isize),
        )
    }
    .as_bool();
    let mut point = POINT::default();
    let cursor = unsafe { GetCursorPos(&mut point) }
        .ok()
        .map(|_| (point.x, point.y));
    let cursor_own_root = cursor.and_then(|_| {
        let root = unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) };
        belongs_to_this_process(root).then_some(root.0 as isize)
    });
    super::log(format_args!(
        "visual_scope phase={phase} capture_request={request} scope={scope} gui_thread={gui_thread} cursor={cursor:?} cursor_own_root_at_observation={cursor_own_root:?} enumerated={} enumeration_complete={complete} truncated={} atomic=false",
        handles.count, handles.truncated
    ));
    let mut inspected = 0;
    for &value in &handles.values[..handles.count] {
        if started.elapsed() >= SNAPSHOT_BUDGET {
            break;
        }
        let hwnd = HWND(value as *mut std::ffi::c_void);
        super::log(format_args!(
            "visual_window phase={phase} capture_request={request} scope={scope} hwnd={value} facts={:?} atomic=false",
            facts(hwnd)
        ));
        inspected += 1;
    }
    super::log(format_args!(
        "visual_scope_end phase={phase} capture_request={request} scope={scope} inspected={inspected} budget_limited={} elapsed_us={}",
        inspected != handles.count, started.elapsed().as_micros()
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::{
        Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
        },
        core::w,
    };

    #[test]
    fn recent_capture_expires_without_a_timer_or_polling() {
        let now = Instant::now();
        assert_eq!(recent_at(None, now), None);
        assert_eq!(recent_at(Some((7, now)), now), Some(7));
        assert_eq!(recent_at(Some((7, now)), now + CONTEXT_DURATION), None);
    }

    #[test]
    fn window_collection_is_bounded_and_reports_truncation() {
        let mut handles = Handles::default();
        for value in 0..MAX_WINDOWS {
            assert!(handles.push(value as isize));
        }
        assert!(!handles.push(99));
        assert!(handles.truncated);
        assert_eq!(handles.count, MAX_WINDOWS);
        assert_eq!(handles.values[MAX_WINDOWS - 1], (MAX_WINDOWS - 1) as isize);
    }

    #[test]
    fn invalid_handles_cannot_supply_native_geometry() {
        assert!(facts(HWND::default()).is_none());
    }

    #[test]
    fn native_observation_does_not_show_move_or_restyle_a_window() {
        let _physical = crate::dpi::PhysicalPixels::enter();
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!(""),
                WS_POPUP,
                -120,
                -80,
                30,
                20,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap();
        let before = facts(hwnd).unwrap();
        let after = facts(hwnd).unwrap();
        unsafe { DestroyWindow(hwnd) }.unwrap();
        assert_eq!(before.window, after.window);
        assert_eq!(before.client, after.client);
        assert_eq!(before.client_origin, after.client_origin);
        assert_eq!(before.ex_style, after.ex_style);
        assert_eq!(before.affinity, after.affinity);
        assert_eq!(before.visible, after.visible);
        assert!(!before.visible);
        assert_eq!(before.window, Some((-120, -80, -90, -60)));
    }
}
