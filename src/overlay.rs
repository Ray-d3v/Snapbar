use std::{
    ffi::c_void,
    mem::size_of,
    ptr::null_mut,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{RecvTimeoutError, SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use async_channel::{Receiver, Sender};
use gpui::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::{
    Foundation::{COLORREF, HWND, POINT, RECT},
    Graphics::{
        Dwm::{
            DWMWA_BORDER_COLOR, DWMWA_CAPTION_BUTTON_BOUNDS, DWMWA_CAPTION_COLOR,
            DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute, DwmSetWindowAttribute,
        },
        Gdi::{
            CreateRectRgn, DeleteObject, ExtCreateRegion, GetDC, GetPixel, HGDIOBJ, HRGN,
            RDH_RECTANGLES, RGNDATA, RGNDATAHEADER, ReleaseDC, SetWindowRgn,
        },
    },
    UI::WindowsAndMessaging::{
        GW_HWNDPREV, GWL_EXSTYLE, GetClientRect, GetWindow, GetWindowLongW, GetWindowRect,
        HWND_NOTOPMOST, HWND_TOPMOST, IsIconic, IsWindow, IsWindowVisible, SW_HIDE,
        SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER,
        SWP_SHOWWINDOW, SetWindowDisplayAffinity, SetWindowLongW, SetWindowPos, ShowWindow,
        WDA_EXCLUDEFROMCAPTURE, WDA_NONE, WINDOW_DISPLAY_AFFINITY, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    },
};

use crate::shutdown::defer_cleanup;

const FOLLOW_INTERVAL: Duration = Duration::from_millis(100);
const PRESENTER_FOLLOW_INTERVAL: Duration = Duration::from_millis(8);
const IDLE_INTERVAL: Duration = Duration::from_millis(500);
const TITLEBAR_SAMPLE_INTERVAL: Duration = Duration::from_millis(350);
const COLOR_SAMPLE_SETTLE_INTERVAL: Duration = Duration::from_millis(50);
const EXPAND_DELAY_MS: u32 = 16;
const COLLAPSE_DELAY_MS: u32 = 50;
const SAFETY_INTERVAL_MS: u32 = 250;
const DWMWA_COLOR_NONE: u32 = 0xffff_fffe;
const DWMWA_COLOR_DEFAULT: u32 = 0xffff_ffff;
const CLR_INVALID: u32 = 0xffff_ffff;
const SUBCLASS_ID: usize = 0x534e_4150;
const TIMER_EXPAND: usize = 0x0053_4e01;
const TIMER_COLLAPSE: usize = 0x0053_4e02;
const TIMER_SAFETY: usize = 0x0053_4e03;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_MOUSELEAVE: u32 = 0x02a3;
const WM_MOUSEACTIVATE: u32 = 0x0021;
const WM_TIMER: u32 = 0x0113;
const WM_SHOWWINDOW: u32 = 0x0018;
const WM_CANCELMODE: u32 = 0x001f;
const WM_NCDESTROY: u32 = 0x0082;
const WM_APP_RESET_DISCLOSURE: u32 = 0x8000 + 0x351;
const WM_APP_REEVALUATE_POINTER: u32 = 0x8000 + 0x352;
const TME_LEAVE: u32 = 0x0000_0002;
const MA_NOACTIVATE: isize = 3;
const WORK_FOLLOW: u32 = 1 << 0;
const WORK_REGION: u32 = 1 << 1;
const DISCLOSURE_PROGRESS_MAX: u32 = 1_000;
const DISCLOSURE_PROGRESS_LIMIT: u32 = 1_044;

fn request_worker_work(pending_work: &AtomicU32, wake_tx: &SyncSender<()>, work: u32) {
    pending_work.fetch_or(work, Ordering::Release);
    let _ = wake_tx.try_send(());
}

fn classify_worker_work(requested_work: u32, follow_due: bool) -> (bool, bool) {
    let follow = follow_due || requested_work & WORK_FOLLOW != 0;
    let region = follow || requested_work & WORK_REGION != 0;
    (follow, region)
}

pub const WINDOW_WIDTH: f32 = 280.0;
pub const WINDOW_HEIGHT: f32 = 48.0;
pub const COLLAPSED_WIDTH: f32 = 92.0;
pub const COLLAPSED_HEIGHT: f32 = 30.0;
pub const EXPANDED_WIDTH: f32 = 272.0;
pub const EXPANDED_HEIGHT: f32 = 46.0;
pub const INLINE_WIDTH: f32 = EXPANDED_WIDTH;
pub const TITLEBAR_FRAME_INSET: f32 = 1.0;
pub const TITLEBAR_SURFACE_HEIGHT: f32 = COLLAPSED_HEIGHT - TITLEBAR_FRAME_INSET;
pub const HOVER_ISLAND_HEIGHT: f32 = EXPANDED_HEIGHT - TITLEBAR_FRAME_INSET;
pub const INLINE_HEIGHT: f32 = TITLEBAR_SURFACE_HEIGHT;
pub const COMPACT_WIDTH: f32 = 46.0;
pub const COMPACT_HEIGHT: f32 = TITLEBAR_SURFACE_HEIGHT;
pub const ISLAND_BOTTOM_RADIUS: f32 = 8.0;
pub const ISLAND_SHOULDER_DEPTH: f32 = 10.0;
pub const ISLAND_SHOULDER_INSET: f32 = 16.0;
pub const ISLAND_DROP: f32 = 16.0;
pub const PRESENTER_COLLAPSED_HEIGHT: f32 = 39.0;
pub const PRESENTER_CORNER_RADIUS: f32 = 16.0;
pub const DEFAULT_TITLEBAR_COLOR: u32 = 0x111111;
pub const PRESENTER_TOOLBAR_COLOR: u32 = 0x202020;

pub fn disclosure_width(progress: f32) -> f32 {
    COLLAPSED_WIDTH
        + (EXPANDED_WIDTH - COLLAPSED_WIDTH)
            * progress.clamp(
                0.0,
                DISCLOSURE_PROGRESS_LIMIT as f32 / DISCLOSURE_PROGRESS_MAX as f32,
            )
}

pub fn disclosure_height(progress: f32) -> f32 {
    TITLEBAR_SURFACE_HEIGHT
        + (HOVER_ISLAND_HEIGHT - TITLEBAR_SURFACE_HEIGHT) * progress.clamp(0.0, 1.0)
}

pub fn presenter_disclosure_height(progress: f32) -> f32 {
    PRESENTER_COLLAPSED_HEIGHT
        + (HOVER_ISLAND_HEIGHT - PRESENTER_COLLAPSED_HEIGHT) * progress.clamp(0.0, 1.0)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverlayPresentation {
    #[default]
    HoverIsland,
    InlineTitlebar,
}

impl OverlayPresentation {
    pub const fn is_inline(self) -> bool {
        matches!(self, Self::InlineTitlebar)
    }

    pub fn from_command_line() -> Self {
        Self::from_arguments(std::env::args())
    }

    fn from_arguments<I, S>(arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        if arguments.into_iter().any(|argument| {
            matches!(
                argument.as_ref().to_str(),
                Some("--inline") | Some("--inline-titlebar")
            )
        }) {
            Self::InlineTitlebar
        } else {
            Self::HoverIsland
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverlayCaptureMode {
    Excluded,
    #[default]
    Recordable,
}

impl OverlayCaptureMode {
    pub fn from_command_line() -> Self {
        Self::from_arguments(std::env::args_os())
    }

    fn from_arguments<I, S>(arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        if arguments.into_iter().any(|argument| {
            matches!(
                argument.as_ref().to_str(),
                Some("--exclude-overlay-from-capture")
            )
        }) {
            Self::Excluded
        } else {
            Self::Recordable
        }
    }

    pub(crate) const fn display_affinity(self) -> WINDOW_DISPLAY_AFFINITY {
        match self {
            Self::Excluded => WDA_EXCLUDEFROMCAPTURE,
            Self::Recordable => WDA_NONE,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverlayMode {
    #[cfg(test)]
    Collapsed,
    Expanded,
    Inline,
    #[cfg(test)]
    Compact,
}

impl OverlayMode {
    pub const fn logical_width(self) -> f32 {
        match self {
            #[cfg(test)]
            Self::Collapsed => COLLAPSED_WIDTH,
            Self::Expanded => EXPANDED_WIDTH,
            Self::Inline => INLINE_WIDTH,
            #[cfg(test)]
            Self::Compact => COMPACT_WIDTH,
        }
    }

    pub const fn logical_height(self) -> f32 {
        match self {
            #[cfg(test)]
            Self::Collapsed => TITLEBAR_SURFACE_HEIGHT,
            Self::Expanded => HOVER_ISLAND_HEIGHT,
            Self::Inline => INLINE_HEIGHT,
            #[cfg(test)]
            Self::Compact => COMPACT_HEIGHT,
        }
    }
}

#[repr(C)]
struct TrackMouseEventRaw {
    cb_size: u32,
    flags: u32,
    hwnd_track: *mut c_void,
    hover_time: u32,
}

type SubclassProc = unsafe extern "system" fn(
    hwnd: *mut c_void,
    message: u32,
    wparam: usize,
    lparam: isize,
    subclass_id: usize,
    reference_data: usize,
) -> isize;

#[link(name = "user32")]
unsafe extern "system" {
    fn ClientToScreen(hwnd: *mut c_void, point: *mut POINT) -> i32;
    fn TrackMouseEvent(event: *mut TrackMouseEventRaw) -> i32;
    fn SetTimer(
        hwnd: *mut c_void,
        timer_id: usize,
        elapsed_ms: u32,
        timer_proc: *mut c_void,
    ) -> usize;
    fn KillTimer(hwnd: *mut c_void, timer_id: usize) -> i32;
    fn PostMessageW(hwnd: *mut c_void, message: u32, wparam: usize, lparam: isize) -> i32;
    fn GetCursorPos(point: *mut POINT) -> i32;
    fn WindowFromPoint(point: POINT) -> *mut c_void;
}

#[link(name = "comctl32")]
unsafe extern "system" {
    fn SetWindowSubclass(
        hwnd: *mut c_void,
        subclass_proc: Option<SubclassProc>,
        subclass_id: usize,
        reference_data: usize,
    ) -> i32;
    fn RemoveWindowSubclass(
        hwnd: *mut c_void,
        subclass_proc: Option<SubclassProc>,
        subclass_id: usize,
    ) -> i32;
    fn DefSubclassProc(hwnd: *mut c_void, message: u32, wparam: usize, lparam: isize) -> isize;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DisclosurePhase {
    Collapsed,
    ExpandPending,
    Expanded,
    CollapsePending,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DisclosureEffects {
    cancel_expand: bool,
    cancel_collapse: bool,
    start_expand: bool,
    start_collapse: bool,
    expanded_changed: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DisclosureMachine {
    phase: DisclosurePhase,
}

impl Default for DisclosureMachine {
    fn default() -> Self {
        Self {
            phase: DisclosurePhase::Collapsed,
        }
    }
}

impl DisclosureMachine {
    fn is_expanded(self) -> bool {
        matches!(
            self.phase,
            DisclosurePhase::Expanded | DisclosurePhase::CollapsePending
        )
    }

    fn pointer_enter(&mut self) -> DisclosureEffects {
        let mut effects = DisclosureEffects {
            cancel_collapse: true,
            ..Default::default()
        };
        match self.phase {
            DisclosurePhase::Collapsed => {
                self.phase = DisclosurePhase::ExpandPending;
                effects.start_expand = true;
            }
            DisclosurePhase::CollapsePending => {
                self.phase = DisclosurePhase::Expanded;
            }
            DisclosurePhase::ExpandPending | DisclosurePhase::Expanded => {}
        }
        effects
    }

    fn pointer_leave(&mut self) -> DisclosureEffects {
        let mut effects = DisclosureEffects {
            cancel_expand: true,
            ..Default::default()
        };
        match self.phase {
            DisclosurePhase::ExpandPending => {
                self.phase = DisclosurePhase::Collapsed;
            }
            DisclosurePhase::Expanded => {
                self.phase = DisclosurePhase::CollapsePending;
                effects.start_collapse = true;
            }
            DisclosurePhase::Collapsed | DisclosurePhase::CollapsePending => {}
        }
        effects
    }

    fn expand_elapsed(&mut self, pointer_over: bool) -> DisclosureEffects {
        let mut effects = DisclosureEffects {
            cancel_expand: true,
            ..Default::default()
        };
        if self.phase == DisclosurePhase::ExpandPending {
            if pointer_over {
                self.phase = DisclosurePhase::Expanded;
                effects.expanded_changed = Some(true);
            } else {
                self.phase = DisclosurePhase::Collapsed;
            }
        }
        effects
    }

    fn collapse_elapsed(&mut self, pointer_over: bool) -> DisclosureEffects {
        let mut effects = DisclosureEffects {
            cancel_collapse: true,
            ..Default::default()
        };
        if self.phase == DisclosurePhase::CollapsePending {
            if pointer_over {
                self.phase = DisclosurePhase::Expanded;
            } else {
                self.phase = DisclosurePhase::Collapsed;
                effects.expanded_changed = Some(false);
            }
        }
        effects
    }

    fn safety_check(&mut self, pointer_over: bool) -> DisclosureEffects {
        match (self.phase, pointer_over) {
            (DisclosurePhase::Expanded, false) => {
                self.phase = DisclosurePhase::CollapsePending;
                DisclosureEffects {
                    start_collapse: true,
                    ..Default::default()
                }
            }
            (DisclosurePhase::CollapsePending, true) => {
                self.phase = DisclosurePhase::Expanded;
                DisclosureEffects {
                    cancel_collapse: true,
                    ..Default::default()
                }
            }
            (DisclosurePhase::ExpandPending, false) => {
                self.phase = DisclosurePhase::Collapsed;
                DisclosureEffects {
                    cancel_expand: true,
                    ..Default::default()
                }
            }
            _ => DisclosureEffects::default(),
        }
    }

    fn reset(&mut self) -> DisclosureEffects {
        let was_expanded = self.is_expanded();
        self.phase = DisclosurePhase::Collapsed;
        DisclosureEffects {
            cancel_expand: true,
            cancel_collapse: true,
            expanded_changed: was_expanded.then_some(false),
            ..Default::default()
        }
    }
}

struct HoverSubclassState {
    machine: DisclosureMachine,
    tracking_leave: bool,
    safety_timer_active: bool,
    expanded: Arc<AtomicBool>,
    compact: Arc<AtomicBool>,
    visible: Arc<AtomicBool>,
    pending_work: Arc<AtomicU32>,
    wake_tx: SyncSender<()>,
    event_tx: Sender<()>,
}

impl HoverSubclassState {
    fn interactive(&self) -> bool {
        self.visible.load(Ordering::Acquire) && !self.compact.load(Ordering::Acquire)
    }

    fn publish(&self) {
        request_worker_work(self.pending_work.as_ref(), &self.wake_tx, WORK_REGION);
        let _ = self.event_tx.try_send(());
    }

    fn start_leave_tracking(&mut self, hwnd: *mut c_void) {
        if self.tracking_leave {
            return;
        }
        let mut event = TrackMouseEventRaw {
            cb_size: std::mem::size_of::<TrackMouseEventRaw>() as u32,
            flags: TME_LEAVE,
            hwnd_track: hwnd,
            hover_time: 0,
        };
        if unsafe { TrackMouseEvent(&mut event) } != 0 {
            self.tracking_leave = true;
        }
    }

    fn forget_leave_tracking(&mut self) {
        // Tracking is associated with the HWND, not with this subclass. Cancelling it here
        // would also cancel GPUI's leave request. A late WM_MOUSELEAVE is harmless because
        // reset() has already returned the disclosure machine to Collapsed.
        self.tracking_leave = false;
    }

    fn apply_effects(&mut self, hwnd: *mut c_void, effects: DisclosureEffects) {
        unsafe {
            if effects.cancel_expand {
                let _ = KillTimer(hwnd, TIMER_EXPAND);
            }
            if effects.cancel_collapse {
                let _ = KillTimer(hwnd, TIMER_COLLAPSE);
            }
            if effects.start_expand {
                let _ = SetTimer(hwnd, TIMER_EXPAND, EXPAND_DELAY_MS, null_mut());
            }
            if effects.start_collapse {
                let _ = SetTimer(hwnd, TIMER_COLLAPSE, COLLAPSE_DELAY_MS, null_mut());
            }
        }

        if let Some(expanded) = effects.expanded_changed {
            if self.expanded.swap(expanded, Ordering::AcqRel) != expanded {
                self.publish();
            }
        }

        let should_run_safety = self.machine.is_expanded();
        if should_run_safety != self.safety_timer_active {
            unsafe {
                if should_run_safety {
                    let _ = SetTimer(hwnd, TIMER_SAFETY, SAFETY_INTERVAL_MS, null_mut());
                } else {
                    let _ = KillTimer(hwnd, TIMER_SAFETY);
                }
            }
            self.safety_timer_active = should_run_safety;
        }
    }

    fn reset(&mut self, hwnd: *mut c_void) {
        self.forget_leave_tracking();
        let effects = self.machine.reset();
        self.apply_effects(hwnd, effects);
    }
}

struct NativeHoverSubclass {
    hwnd: isize,
    state_ptr: usize,
}

impl NativeHoverSubclass {
    fn install(
        hwnd: HWND,
        expanded: Arc<AtomicBool>,
        compact: Arc<AtomicBool>,
        visible: Arc<AtomicBool>,
        pending_work: Arc<AtomicU32>,
        wake_tx: SyncSender<()>,
        event_tx: Sender<()>,
    ) -> Option<Self> {
        let state = Box::new(HoverSubclassState {
            machine: DisclosureMachine::default(),
            tracking_leave: false,
            safety_timer_active: false,
            expanded,
            compact,
            visible,
            pending_work,
            wake_tx,
            event_tx,
        });
        let state_ptr = Box::into_raw(state) as usize;
        let installed = unsafe {
            SetWindowSubclass(hwnd.0, Some(overlay_subclass_proc), SUBCLASS_ID, state_ptr)
        };
        if installed == 0 {
            unsafe {
                drop(Box::from_raw(state_ptr as *mut HoverSubclassState));
            }
            return None;
        }
        Some(Self {
            hwnd: hwnd.0 as isize,
            state_ptr,
        })
    }

    fn reset(&self) {
        post_overlay_message(HWND(self.hwnd as *mut c_void), WM_APP_RESET_DISCLOSURE);
    }
}

impl Drop for NativeHoverSubclass {
    fn drop(&mut self) {
        let hwnd = self.hwnd as *mut c_void;
        unsafe {
            let state = &mut *(self.state_ptr as *mut HoverSubclassState);
            state.reset(hwnd);
            let _ = RemoveWindowSubclass(hwnd, Some(overlay_subclass_proc), SUBCLASS_ID);
            drop(Box::from_raw(self.state_ptr as *mut HoverSubclassState));
        }
    }
}

unsafe extern "system" fn overlay_subclass_proc(
    hwnd: *mut c_void,
    message: u32,
    wparam: usize,
    lparam: isize,
    _subclass_id: usize,
    reference_data: usize,
) -> isize {
    let state = unsafe { &mut *(reference_data as *mut HoverSubclassState) };
    match message {
        WM_MOUSEACTIVATE => return MA_NOACTIVATE,
        WM_MOUSEMOVE => {
            if state.interactive() {
                state.start_leave_tracking(hwnd);
                let effects = state.machine.pointer_enter();
                state.apply_effects(hwnd, effects);
            } else {
                state.reset(hwnd);
            }
        }
        WM_MOUSELEAVE => {
            state.tracking_leave = false;
            let effects = state.machine.pointer_leave();
            state.apply_effects(hwnd, effects);
        }
        WM_TIMER => match wparam {
            TIMER_EXPAND => {
                let pointer_over = pointer_is_over(HWND(hwnd));
                if pointer_over {
                    state.start_leave_tracking(hwnd);
                }
                let effects = state
                    .machine
                    .expand_elapsed(pointer_over && state.interactive());
                state.apply_effects(hwnd, effects);
            }
            TIMER_COLLAPSE => {
                let pointer_over = pointer_is_over(HWND(hwnd)) && state.interactive();
                if pointer_over {
                    state.start_leave_tracking(hwnd);
                }
                let effects = state.machine.collapse_elapsed(pointer_over);
                state.apply_effects(hwnd, effects);
            }
            TIMER_SAFETY => {
                let pointer_over = pointer_is_over(HWND(hwnd)) && state.interactive();
                if pointer_over {
                    state.start_leave_tracking(hwnd);
                }
                let effects = state.machine.safety_check(pointer_over);
                state.apply_effects(hwnd, effects);
            }
            _ => {}
        },
        WM_APP_RESET_DISCLOSURE | WM_CANCELMODE => state.reset(hwnd),
        WM_APP_REEVALUATE_POINTER => {
            if !state.interactive() {
                state.reset(hwnd);
            } else if pointer_is_over(HWND(hwnd)) {
                state.start_leave_tracking(hwnd);
                let effects = state.machine.pointer_enter();
                state.apply_effects(hwnd, effects);
            } else {
                let effects = state.machine.pointer_leave();
                state.apply_effects(hwnd, effects);
            }
        }
        WM_SHOWWINDOW if wparam == 0 => state.reset(hwnd),
        WM_NCDESTROY => state.reset(hwnd),
        _ => {}
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

fn pointer_is_over(hwnd: HWND) -> bool {
    let mut point = POINT::default();
    if unsafe { GetCursorPos(&mut point) } == 0 {
        return false;
    }
    unsafe { WindowFromPoint(point) == hwnd.0 }
}

fn post_overlay_message(hwnd: HWND, message: u32) {
    unsafe {
        let _ = PostMessageW(hwnd.0, message, 0, 0);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RectI {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl RectI {
    fn width(self) -> i32 {
        self.right.saturating_sub(self.left)
    }

    fn height(self) -> i32 {
        self.bottom.saturating_sub(self.top)
    }

    fn center_x(self) -> i32 {
        self.left + self.width() / 2
    }

    fn offset(self, x: i32, y: i32) -> Self {
        Self {
            left: self.left.saturating_add(x),
            top: self.top.saturating_add(y),
            right: self.right.saturating_add(x),
            bottom: self.bottom.saturating_add(y),
        }
    }
}

impl From<RECT> for RectI {
    fn from(value: RECT) -> Self {
        Self {
            left: value.left,
            top: value.top,
            right: value.right,
            bottom: value.bottom,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverlayPlacement {
    Hidden,
    Visible { x: i32, y: i32, compact: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverlayZOrderAnchor {
    Topmost,
    NotTopmost,
    After(isize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowMetrics {
    window_rect: RectI,
    client_screen_left: i32,
    client_screen_top: i32,
    client_width: i32,
    client_height: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowRegion {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    shape: WindowRegionShape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowRegionShape {
    Rectangle,
    Island {
        shoulder_start: i32,
        shoulder_depth: i32,
        shoulder_inset: i32,
        bottom_radius: i32,
    },
    RoundedRectangle {
        corner_radius: i32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CaptionGeometry {
    band: RectI,
    buttons_left: i32,
}

fn colorref_to_rgb(color: COLORREF) -> u32 {
    let color = color.0;
    let red = color & 0xff;
    let green = (color >> 8) & 0xff;
    let blue = (color >> 16) & 0xff;
    (red << 16) | (green << 8) | blue
}

fn color_distance(left: u32, right: u32) -> u32 {
    let channel_distance =
        |shift: u32| ((left >> shift) & 0xff_u32).abs_diff((right >> shift) & 0xff_u32);
    channel_distance(16) + channel_distance(8) + channel_distance(0)
}

fn representative_color(samples: &[u32]) -> Option<u32> {
    samples.iter().copied().min_by_key(|candidate| {
        samples
            .iter()
            .map(|sample| color_distance(*candidate, *sample) as u64)
            .sum::<u64>()
    })
}

fn colors_materially_differ(left: u32, right: u32) -> bool {
    [16_u32, 8, 0]
        .into_iter()
        .any(|shift| ((left >> shift) & 0xff_u32).abs_diff((right >> shift) & 0xff_u32) >= 3)
}

fn push_segment_samples(points: &mut Vec<i32>, left: i32, right: i32) {
    if right < left {
        return;
    }
    let span = right.saturating_sub(left);
    for numerator in [0, 1, 2, 3] {
        points.push(left.saturating_add(span.saturating_mul(numerator) / 3));
    }
}

fn titlebar_sample_points(caption: CaptionGeometry) -> Vec<(i32, i32)> {
    let band = caption.band;
    let center = band.center_x();
    let scale = (band.height() as f32 / COLLAPSED_HEIGHT).clamp(0.75, 4.0);
    let island_exclusion = (WINDOW_WIDTH * scale / 2.0).ceil() as i32 + 18;
    let outer_inset = (12.0 * scale).round() as i32;
    let mut x_positions = Vec::with_capacity(8);
    push_segment_samples(
        &mut x_positions,
        band.left.saturating_add(outer_inset),
        center.saturating_sub(island_exclusion),
    );
    push_segment_samples(
        &mut x_positions,
        center.saturating_add(island_exclusion),
        caption
            .buttons_left
            .min(band.right)
            .saturating_sub(outer_inset),
    );

    if x_positions.is_empty() {
        push_segment_samples(
            &mut x_positions,
            band.left.saturating_add(outer_inset),
            caption
                .buttons_left
                .min(band.right)
                .saturating_sub(outer_inset),
        );
    }

    let inner_top = band.top.saturating_add(1);
    let inner_bottom = band.bottom.saturating_sub(2).max(inner_top);
    let first_y = (band.top + band.height() / 3).clamp(inner_top, inner_bottom);
    let second_y = (band.top + band.height() * 2 / 3).clamp(inner_top, inner_bottom);
    let mut points = Vec::with_capacity(x_positions.len() * 2);
    for x in x_positions {
        points.push((x, first_y));
        if second_y != first_y {
            points.push((x, second_y));
        }
    }
    points
}

fn sample_screen_colors(points: &[(i32, i32)]) -> Vec<u32> {
    let screen_dc = unsafe { GetDC(None) };
    if screen_dc.0.is_null() {
        return Vec::new();
    }

    let samples = points
        .iter()
        .filter_map(|(x, y)| {
            let color = unsafe { GetPixel(screen_dc, *x, *y) };
            (color.0 != CLR_INVALID).then(|| colorref_to_rgb(color))
        })
        .collect();
    unsafe {
        let _ = ReleaseDC(None, screen_dc);
    }
    samples
}

fn dwm_caption_color(hwnd: HWND) -> Option<u32> {
    let mut color = COLORREF::default();
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            &mut color as *mut COLORREF as *mut c_void,
            std::mem::size_of::<COLORREF>() as u32,
        )
        .ok()?;
    }
    (!matches!(color.0, DWMWA_COLOR_DEFAULT | DWMWA_COLOR_NONE)).then(|| colorref_to_rgb(color))
}

fn sample_titlebar_color(hwnd: HWND, caption_height: i32) -> Option<u32> {
    let window = get_window_rect(hwnd)?;
    let frame = extended_frame_bounds(hwnd).unwrap_or(window);
    let caption = caption_geometry(window, frame, caption_button_bounds(hwnd), caption_height)?;
    representative_color(&sample_screen_colors(&titlebar_sample_points(caption)))
        .or_else(|| dwm_caption_color(hwnd))
}

#[derive(Clone)]
pub struct DisclosureProgressPublisher {
    progress: Arc<AtomicU32>,
    pending_work: Arc<AtomicU32>,
    wake_tx: SyncSender<()>,
}

impl DisclosureProgressPublisher {
    pub fn publish(&self, progress: f32) {
        let next = (progress.max(0.0) * DISCLOSURE_PROGRESS_MAX as f32)
            .round()
            .clamp(0.0, DISCLOSURE_PROGRESS_LIMIT as f32) as u32;
        if self.progress.swap(next, Ordering::AcqRel) != next {
            request_worker_work(self.pending_work.as_ref(), &self.wake_tx, WORK_REGION);
        }
    }
}

fn follow_interval(target_id: u32, presenter_attached: bool) -> Duration {
    if presenter_attached {
        PRESENTER_FOLLOW_INTERVAL
    } else if target_id == 0 {
        IDLE_INTERVAL
    } else {
        FOLLOW_INTERVAL
    }
}

fn full_follow_due(
    last_full_follow: Option<Instant>,
    now: Instant,
    interval: Duration,
    anchor_changed: bool,
) -> bool {
    anchor_changed
        || match last_full_follow {
            Some(last) => now.saturating_duration_since(last) >= interval,
            None => true,
        }
}

fn remaining_follow_wait(
    last_full_follow: Option<Instant>,
    now: Instant,
    interval: Duration,
) -> Duration {
    match last_full_follow {
        Some(last) => interval.saturating_sub(now.saturating_duration_since(last)),
        None => Duration::ZERO,
    }
}

fn region_update_redraw(disclosure_progress: u32, structural_change: bool) -> bool {
    // GPUI presents the intermediate pixels itself; request a full native redraw only
    // when the clip settles or unrelated window geometry changes.
    structural_change || matches!(disclosure_progress, 0 | DISCLOSURE_PROGRESS_MAX)
}

fn disclosure_is_settled(
    expanded: bool,
    disclosure_progress: u32,
    last_progress_change: Instant,
    now: Instant,
) -> bool {
    let at_target = if expanded {
        disclosure_progress == DISCLOSURE_PROGRESS_MAX
    } else {
        disclosure_progress == 0
    };
    at_target && now.saturating_duration_since(last_progress_change) >= COLOR_SAMPLE_SETTLE_INTERVAL
}

pub struct TeamsWindowFollower {
    overlay_hwnd: isize,
    capture_mode: OverlayCaptureMode,
    target_id: Arc<AtomicU32>,
    presenter_toolbar_id: Arc<AtomicU32>,
    titlebar_color: Arc<AtomicU32>,
    expanded: Arc<AtomicBool>,
    disclosure_progress: Arc<AtomicU32>,
    compact: Arc<AtomicBool>,
    visible: Arc<AtomicBool>,
    event_rx: Receiver<()>,
    pending_work: Arc<AtomicU32>,
    wake_tx: SyncSender<()>,
    native_hover: Option<NativeHoverSubclass>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl TeamsWindowFollower {
    pub fn start(
        window: &Window,
        presentation: OverlayPresentation,
        capture_mode: OverlayCaptureMode,
    ) -> Option<Self> {
        let overlay_hwnd = window_hwnd(window)?;
        configure_overlay_window(overlay_hwnd);
        unsafe {
            let _ = ShowWindow(overlay_hwnd, SW_HIDE);
        }

        let target_id = Arc::new(AtomicU32::new(0));
        let presenter_toolbar_id = Arc::new(AtomicU32::new(0));
        let titlebar_color = Arc::new(AtomicU32::new(DEFAULT_TITLEBAR_COLOR));
        let expanded = Arc::new(AtomicBool::new(false));
        let disclosure_progress = Arc::new(AtomicU32::new(0));
        let compact = Arc::new(AtomicBool::new(false));
        let visible = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let pending_work = Arc::new(AtomicU32::new(WORK_FOLLOW | WORK_REGION));
        let (wake_tx, wake_rx) = sync_channel(1);
        let (event_tx, event_rx) = async_channel::bounded(1);
        let native_hover = Some(NativeHoverSubclass::install(
            overlay_hwnd,
            Arc::clone(&expanded),
            Arc::clone(&compact),
            Arc::clone(&visible),
            Arc::clone(&pending_work),
            wake_tx.clone(),
            event_tx.clone(),
        )?);

        let thread_target_id = Arc::clone(&target_id);
        let thread_presenter_toolbar_id = Arc::clone(&presenter_toolbar_id);
        let thread_titlebar_color = Arc::clone(&titlebar_color);
        let thread_expanded = Arc::clone(&expanded);
        let thread_disclosure_progress = Arc::clone(&disclosure_progress);
        let thread_compact = Arc::clone(&compact);
        let thread_visible = Arc::clone(&visible);
        let thread_stop = Arc::clone(&stop);
        let thread_pending_work = Arc::clone(&pending_work);
        let thread_event_tx = event_tx.clone();
        let thread_presentation = presentation;
        let display_affinity = capture_mode.display_affinity();
        let overlay_value = overlay_hwnd.0 as isize;
        let worker = thread::Builder::new()
            .name("snapbar-window-follow".to_string())
            .spawn(move || {
                let overlay_hwnd = HWND(overlay_value as *mut c_void);
                unsafe {
                    let _ = SetWindowDisplayAffinity(overlay_hwnd, display_affinity);
                }

                let mut previous_placement = None;
                let mut previous_region_state = None;
                let mut previous_visible = false;
                let mut previous_compact = false;
                let mut previous_target_id = 0;
                let mut previous_presenter_toolbar_id = 0;
                let mut cached_overlay_metrics = None;
                let mut last_full_follow = None;
                let mut previous_disclosure_progress = 0;
                let mut last_progress_change = Instant::now() - COLOR_SAMPLE_SETTLE_INTERVAL;
                let mut last_titlebar_sample = Instant::now() - TITLEBAR_SAMPLE_INTERVAL;
                while !thread_stop.load(Ordering::Acquire) {
                    let requested_work = thread_pending_work.swap(0, Ordering::AcqRel);
                    let target_id = thread_target_id.load(Ordering::Acquire);
                    let presenter_toolbar_id = thread_presenter_toolbar_id.load(Ordering::Acquire);
                    let presenter_attached = presenter_toolbar_id != 0;
                    let target_changed = target_id != previous_target_id;
                    let presenter_changed = presenter_toolbar_id != previous_presenter_toolbar_id;
                    let anchor_changed = target_changed || presenter_changed;
                    let interval = follow_interval(target_id, presenter_attached);
                    let follow_started = Instant::now();
                    // GPUI can publish at the monitor refresh rate. Keep target/DWM/z-order
                    // queries on an absolute cadence so frame wakes only reshape the exact
                    // native hit region between full follow passes.
                    let follow_due =
                        full_follow_due(last_full_follow, follow_started, interval, anchor_changed);
                    let (run_full_follow, run_region_update) =
                        classify_worker_work(requested_work, follow_due);
                    let mut structural_change = false;

                    if run_full_follow {
                        let placement = if presenter_attached {
                            let presenter_hwnd = HWND(presenter_toolbar_id as usize as *mut c_void);
                            desired_presenter_placement(
                                overlay_hwnd,
                                presenter_hwnd,
                                thread_presentation,
                            )
                        } else if target_id == 0 {
                            OverlayPlacement::Hidden
                        } else {
                            let target_hwnd = HWND(target_id as usize as *mut c_void);
                            desired_placement(overlay_hwnd, target_hwnd, thread_presentation)
                        };

                        let visible_now = matches!(placement, OverlayPlacement::Visible { .. });
                        let compact_now =
                            matches!(placement, OverlayPlacement::Visible { compact: true, .. });
                        thread_visible.store(visible_now, Ordering::Release);
                        thread_compact.store(compact_now, Ordering::Release);

                        let visibility_changed = visible_now != previous_visible;
                        let compact_changed = compact_now != previous_compact;
                        if visibility_changed || compact_changed || anchor_changed {
                            if !visible_now || compact_now {
                                thread_expanded.store(false, Ordering::Release);
                                thread_disclosure_progress.store(0, Ordering::Release);
                                post_overlay_message(overlay_hwnd, WM_APP_RESET_DISCLOSURE);
                            }
                            let _ = thread_event_tx.try_send(());
                        }

                        let placement_changed = previous_placement != Some(placement);
                        if placement_changed {
                            apply_placement(overlay_hwnd, placement);
                            if visible_now {
                                post_overlay_message(overlay_hwnd, WM_APP_REEVALUATE_POINTER);
                            }
                            previous_placement = Some(placement);
                        }

                        if visible_now {
                            let anchor_hwnd = if presenter_attached {
                                HWND(presenter_toolbar_id as usize as *mut c_void)
                            } else {
                                HWND(target_id as usize as *mut c_void)
                            };
                            sync_overlay_z_order(overlay_hwnd, anchor_hwnd);
                        }

                        let next_metrics = window_metrics(overlay_hwnd);
                        let metrics_changed = next_metrics != cached_overlay_metrics;
                        cached_overlay_metrics = next_metrics;
                        structural_change = anchor_changed
                            || visibility_changed
                            || compact_changed
                            || placement_changed
                            || metrics_changed;
                        previous_visible = visible_now;
                        previous_compact = compact_now;
                        previous_target_id = target_id;
                        previous_presenter_toolbar_id = presenter_toolbar_id;
                        last_full_follow = Some(follow_started);
                    }

                    let disclosure_progress = if previous_visible && !previous_compact {
                        thread_disclosure_progress.load(Ordering::Acquire)
                    } else {
                        thread_disclosure_progress.store(0, Ordering::Release);
                        0
                    };
                    let progress_observed_at = Instant::now();
                    if disclosure_progress != previous_disclosure_progress {
                        previous_disclosure_progress = disclosure_progress;
                        last_progress_change = progress_observed_at;
                    }

                    if run_region_update {
                        match cached_overlay_metrics.map(|metrics| {
                            window_region_for_attachment(
                                metrics,
                                thread_presentation,
                                previous_compact,
                                presenter_attached,
                                disclosure_progress,
                            )
                        }) {
                            Some(region) if previous_region_state != Some(region) => {
                                let redraw =
                                    region_update_redraw(disclosure_progress, structural_change);
                                if apply_window_region(overlay_hwnd, region, redraw) {
                                    previous_region_state = Some(region);
                                }
                            }
                            Some(_) => {}
                            None => previous_region_state = None,
                        }
                    }

                    if run_full_follow
                        && previous_visible
                        && (anchor_changed
                            || last_titlebar_sample.elapsed() >= TITLEBAR_SAMPLE_INTERVAL)
                        && (presenter_attached
                            || disclosure_is_settled(
                                thread_expanded.load(Ordering::Acquire),
                                disclosure_progress,
                                last_progress_change,
                                progress_observed_at,
                            ))
                    {
                        // GetPixel can synchronize with desktop composition. Defer it until
                        // the integer disclosure geometry has stayed at its target long
                        // enough that it cannot interrupt an active transition.
                        let sampled_color = if presenter_attached {
                            Some(PRESENTER_TOOLBAR_COLOR)
                        } else {
                            let target_hwnd = HWND(target_id as usize as *mut c_void);
                            let caption_height = cached_overlay_metrics
                                .map(nominal_caption_height)
                                .unwrap_or(32);
                            sample_titlebar_color(target_hwnd, caption_height)
                        };
                        if let Some(sampled_color) = sampled_color {
                            let current_color = thread_titlebar_color.load(Ordering::Acquire);
                            if colors_materially_differ(current_color, sampled_color) {
                                thread_titlebar_color.store(sampled_color, Ordering::Release);
                                let _ = thread_event_tx.try_send(());
                            }
                        }
                        last_titlebar_sample = Instant::now();
                    }

                    let wait = remaining_follow_wait(
                        last_full_follow,
                        Instant::now(),
                        follow_interval(target_id, presenter_attached),
                    );
                    match wake_rx.recv_timeout(wait) {
                        Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                thread_visible.store(false, Ordering::Release);
                thread_expanded.store(false, Ordering::Release);
                let _ = thread_event_tx.try_send(());
                apply_placement(overlay_hwnd, OverlayPlacement::Hidden);
            })
            .ok()?;

        Some(Self {
            overlay_hwnd: overlay_hwnd.0 as isize,
            capture_mode,
            target_id,
            presenter_toolbar_id,
            titlebar_color,
            expanded,
            disclosure_progress,
            compact,
            visible,
            event_rx,
            pending_work,
            wake_tx,
            native_hover,
            stop,
            worker: Some(worker),
        })
    }

    fn wake(&self, work: u32) {
        request_worker_work(self.pending_work.as_ref(), &self.wake_tx, work);
    }

    pub fn subscribe(&self) -> Receiver<()> {
        self.event_rx.clone()
    }

    pub fn set_target(&self, target_id: Option<u32>) {
        let next = target_id.unwrap_or_default();
        if self.target_id.swap(next, Ordering::AcqRel) != next {
            if let Some(native_hover) = &self.native_hover {
                native_hover.reset();
            }
            self.expanded.store(false, Ordering::Release);
            self.disclosure_progress.store(0, Ordering::Release);
            self.wake(WORK_FOLLOW | WORK_REGION);
        }
    }

    pub fn set_presenter_toolbar(&self, presenter_toolbar_id: Option<u32>) {
        let next = presenter_toolbar_id.unwrap_or_default();
        if self.presenter_toolbar_id.swap(next, Ordering::AcqRel) != next {
            if let Some(native_hover) = &self.native_hover {
                native_hover.reset();
            }
            self.expanded.store(false, Ordering::Release);
            self.disclosure_progress.store(0, Ordering::Release);
            self.wake(WORK_FOLLOW | WORK_REGION);
        }
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded.load(Ordering::Acquire)
    }

    pub fn titlebar_color(&self) -> u32 {
        self.titlebar_color.load(Ordering::Acquire)
    }

    pub fn disclosure_progress_publisher(&self) -> DisclosureProgressPublisher {
        DisclosureProgressPublisher {
            progress: Arc::clone(&self.disclosure_progress),
            pending_work: Arc::clone(&self.pending_work),
            wake_tx: self.wake_tx.clone(),
        }
    }

    pub fn disclosure_progress(&self) -> f32 {
        self.disclosure_progress.load(Ordering::Acquire) as f32 / DISCLOSURE_PROGRESS_MAX as f32
    }

    pub fn is_compact(&self) -> bool {
        self.compact.load(Ordering::Acquire)
    }

    pub fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Acquire)
    }

    pub fn begin_shutdown(&self) {
        self.stop.store(true, Ordering::Release);
        self.target_id.store(0, Ordering::Release);
        self.presenter_toolbar_id.store(0, Ordering::Release);
        self.expanded.store(false, Ordering::Release);
        self.disclosure_progress.store(0, Ordering::Release);
        self.compact.store(false, Ordering::Release);
        self.visible.store(false, Ordering::Release);
        self.wake(WORK_FOLLOW | WORK_REGION);

        let hwnd = HWND(self.overlay_hwnd as *mut c_void);
        post_overlay_message(hwnd, WM_APP_RESET_DISCLOSURE);
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }

    pub fn exclude_overlay_from_capture(&self) -> Option<TemporaryOverlayCaptureExclusion> {
        let hwnd = HWND(self.overlay_hwnd as *mut c_void);
        unsafe {
            SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE).ok()?;
        }
        Some(TemporaryOverlayCaptureExclusion {
            hwnd: self.overlay_hwnd,
            restore: self.capture_mode.display_affinity(),
        })
    }
}

pub struct TemporaryOverlayCaptureExclusion {
    hwnd: isize,
    restore: WINDOW_DISPLAY_AFFINITY,
}

impl Drop for TemporaryOverlayCaptureExclusion {
    fn drop(&mut self) {
        let hwnd = HWND(self.hwnd as *mut c_void);
        unsafe {
            if IsWindow(Some(hwnd)).as_bool() {
                let _ = SetWindowDisplayAffinity(hwnd, self.restore);
            }
        }
    }
}

impl Drop for TeamsWindowFollower {
    fn drop(&mut self) {
        self.begin_shutdown();
        if let Some(worker) = self.worker.take() {
            defer_cleanup("snapbar-window-follow-stop", move || {
                let _ = worker.join();
            });
        }
        self.native_hover.take();
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
        window_rect: window_rect.into(),
        client_screen_left: client_origin.x,
        client_screen_top: client_origin.y,
        client_width,
        client_height,
    })
}

fn scale_logical(value: f32, actual: i32, logical: f32) -> i32 {
    ((value * actual as f32) / logical).round() as i32
}

fn nominal_caption_height(overlay: WindowMetrics) -> i32 {
    scale_logical(COLLAPSED_HEIGHT, overlay.client_height, WINDOW_HEIGHT).max(24)
}

fn nominal_island_drop(overlay: WindowMetrics) -> i32 {
    let caption_height = nominal_caption_height(overlay);
    // Scale the two logical edges before subtracting them. Scaling the 16px size
    // independently can round away the reserved top-outline pixel at custom DPIs.
    let caption_and_drop = scale_logical(
        COLLAPSED_HEIGHT + ISLAND_DROP,
        overlay.client_height,
        WINDOW_HEIGHT,
    );
    caption_and_drop.saturating_sub(caption_height)
}

fn surface_rect_for_size(
    width: i32,
    height: i32,
    logical_width: f32,
    logical_height: f32,
) -> RectI {
    let surface_width = scale_logical(logical_width, width, WINDOW_WIDTH).clamp(1, width);
    let surface_height = scale_logical(logical_height, height, WINDOW_HEIGHT).clamp(1, height);
    let expanded_height = scale_logical(EXPANDED_HEIGHT, height, WINDOW_HEIGHT).clamp(1, height);
    let surface_left = width / 2 - surface_width / 2;
    // Every mode starts at the title-bar top edge. The idle caption cells end at the
    // caption bottom, while only the expanded island uses the reserved drop below it.
    let surface_top = (height - expanded_height) / 2;

    RectI {
        left: surface_left,
        top: surface_top,
        right: surface_left + surface_width,
        bottom: surface_top + surface_height,
    }
}

fn surface_rect(width: i32, height: i32, mode: OverlayMode) -> RectI {
    surface_rect_for_size(width, height, mode.logical_width(), mode.logical_height())
}

fn compact_surface_rect_for_presentation(
    width: i32,
    height: i32,
    presentation: OverlayPresentation,
) -> RectI {
    surface_rect_for_size(
        width,
        height,
        COMPACT_WIDTH,
        if presentation.is_inline() {
            INLINE_HEIGHT
        } else {
            COMPACT_HEIGHT
        },
    )
}

#[cfg(test)]
fn disclosure_surface_rect(width: i32, height: i32, progress: u32) -> RectI {
    disclosure_surface_rect_for_presentation(
        width,
        height,
        progress,
        OverlayPresentation::HoverIsland,
    )
}

#[cfg(test)]
fn disclosure_surface_rect_for_presentation(
    width: i32,
    height: i32,
    progress: u32,
    presentation: OverlayPresentation,
) -> RectI {
    disclosure_surface_rect_for_attachment(width, height, progress, presentation, false)
}

fn disclosure_surface_rect_for_attachment(
    width: i32,
    height: i32,
    progress: u32,
    presentation: OverlayPresentation,
    presenter_attached: bool,
) -> RectI {
    let progress = progress.min(DISCLOSURE_PROGRESS_LIMIT) as f32 / DISCLOSURE_PROGRESS_MAX as f32;
    let logical_height = if presenter_attached {
        presenter_disclosure_height(progress)
    } else if presentation.is_inline() {
        INLINE_HEIGHT
    } else {
        disclosure_height(progress)
    };
    surface_rect_for_size(width, height, disclosure_width(progress), logical_height)
}

#[cfg(test)]
fn hover_rect(width: i32, height: i32, mode: OverlayMode) -> RectI {
    surface_rect(width, height, mode)
}

#[cfg(test)]
fn window_region(metrics: WindowMetrics, mode: OverlayMode) -> WindowRegion {
    match mode {
        OverlayMode::Collapsed => {
            window_region_for_progress(metrics, OverlayPresentation::HoverIsland, false, 0)
        }
        OverlayMode::Expanded => window_region_for_progress(
            metrics,
            OverlayPresentation::HoverIsland,
            false,
            DISCLOSURE_PROGRESS_MAX,
        ),
        OverlayMode::Inline => window_region_for_progress(
            metrics,
            OverlayPresentation::InlineTitlebar,
            false,
            DISCLOSURE_PROGRESS_MAX,
        ),
        OverlayMode::Compact => {
            window_region_for_progress(metrics, OverlayPresentation::HoverIsland, true, 0)
        }
    }
}

#[cfg(test)]
fn window_region_for_progress(
    metrics: WindowMetrics,
    presentation: OverlayPresentation,
    compact: bool,
    progress: u32,
) -> WindowRegion {
    window_region_for_attachment(metrics, presentation, compact, false, progress)
}

fn window_region_for_attachment(
    metrics: WindowMetrics,
    presentation: OverlayPresentation,
    compact: bool,
    presenter_attached: bool,
    progress: u32,
) -> WindowRegion {
    let client_left = metrics
        .client_screen_left
        .saturating_sub(metrics.window_rect.left);
    let client_top = metrics
        .client_screen_top
        .saturating_sub(metrics.window_rect.top);
    let progress = progress.min(DISCLOSURE_PROGRESS_LIMIT);
    let hover = if compact {
        if presenter_attached {
            surface_rect_for_size(
                metrics.client_width,
                metrics.client_height,
                COMPACT_WIDTH,
                PRESENTER_COLLAPSED_HEIGHT,
            )
        } else {
            compact_surface_rect_for_presentation(
                metrics.client_width,
                metrics.client_height,
                presentation,
            )
        }
    } else {
        disclosure_surface_rect_for_attachment(
            metrics.client_width,
            metrics.client_height,
            progress,
            presentation,
            presenter_attached,
        )
    };
    let normalized = progress as f32 / DISCLOSURE_PROGRESS_MAX as f32;
    let shape = if presenter_attached {
        let corner_radius = scale_logical(
            PRESENTER_CORNER_RADIUS,
            metrics.client_height,
            WINDOW_HEIGHT,
        )
        .clamp(0, hover.width().min(hover.height()) / 2);
        WindowRegionShape::RoundedRectangle { corner_radius }
    } else if compact || presentation.is_inline() || progress == 0 {
        WindowRegionShape::Rectangle
    } else {
        let caption_height = scale_logical(
            TITLEBAR_SURFACE_HEIGHT,
            metrics.client_height,
            WINDOW_HEIGHT,
        )
        .clamp(1, hover.height());
        let animated_drop = hover.height().saturating_sub(caption_height);
        let bottom_radius = scale_logical(
            ISLAND_BOTTOM_RADIUS * normalized,
            metrics.client_height,
            WINDOW_HEIGHT,
        )
        .clamp(0, animated_drop);
        let shoulder_depth = scale_logical(
            ISLAND_SHOULDER_DEPTH * normalized,
            metrics.client_height,
            WINDOW_HEIGHT,
        )
        // Keep the concave root independent from the bottom corner radius. The
        // two curves can overlap near their tangents without shortening the
        // shoulder when the lower corners are made rounder.
        .clamp(0, animated_drop);
        let shoulder_inset = scale_logical(
            ISLAND_SHOULDER_INSET * normalized,
            metrics.client_width,
            WINDOW_WIDTH,
        )
        .clamp(0, hover.width().saturating_sub(1) / 2);
        WindowRegionShape::Island {
            shoulder_start: caption_height,
            shoulder_depth,
            shoulder_inset,
            bottom_radius,
        }
    };
    WindowRegion {
        left: client_left.saturating_add(hover.left),
        top: client_top.saturating_add(hover.top),
        right: client_left.saturating_add(hover.right),
        bottom: client_top.saturating_add(hover.bottom),
        shape,
    }
}

fn island_row_inset(
    row: i32,
    width: i32,
    height: i32,
    shoulder_start: i32,
    shoulder_depth: i32,
    shoulder_inset: i32,
    bottom_radius: i32,
) -> i32 {
    let shoulder_row = row.saturating_sub(shoulder_start);
    let mut inset = if row < shoulder_start {
        0
    } else if shoulder_depth <= 1 || shoulder_row >= shoulder_depth {
        shoulder_inset
    } else {
        let phase = shoulder_row as f32 / (shoulder_depth - 1) as f32;
        // A quarter-circle ease-out gives the join a horizontal tangent against
        // the caption edge and a vertical tangent as it reaches the island body.
        let eased = (1.0 - (1.0 - phase) * (1.0 - phase)).sqrt();
        (shoulder_inset as f32 * eased).round() as i32
    };

    if bottom_radius > 0 {
        let corner_start = height.saturating_sub(bottom_radius);
        if row >= corner_start {
            let phase = if bottom_radius <= 1 {
                1.0
            } else {
                (row - corner_start) as f32 / (bottom_radius - 1) as f32
            };
            let circle = (1.0 - phase * phase).max(0.0).sqrt();
            let corner_inset = (bottom_radius as f32 * (1.0 - circle)).round() as i32;
            inset = shoulder_inset.saturating_add(corner_inset);
        }
    }

    inset.clamp(0, width.saturating_sub(1) / 2)
}

fn rounded_rectangle_row_inset(row: i32, width: i32, height: i32, corner_radius: i32) -> i32 {
    let corner_radius = corner_radius.clamp(0, width.min(height) / 2);
    if corner_radius <= 0 {
        return 0;
    }

    let edge_distance = row.min(height.saturating_sub(1).saturating_sub(row));
    if edge_distance >= corner_radius {
        0
    } else {
        let phase = if corner_radius <= 1 {
            1.0
        } else {
            (corner_radius - 1 - edge_distance) as f32 / (corner_radius - 1) as f32
        };
        let circle = (1.0 - phase * phase).max(0.0).sqrt();
        (corner_radius as f32 * (1.0 - circle)).round() as i32
    }
}

fn coalesced_row_rectangles(
    desired: WindowRegion,
    mut row_inset: impl FnMut(i32, i32, i32) -> i32,
) -> Vec<RectI> {
    let width = desired.right.saturating_sub(desired.left);
    let height = desired.bottom.saturating_sub(desired.top);
    if width <= 0 || height <= 0 {
        return Vec::new();
    }

    let mut rectangles: Vec<RectI> = Vec::with_capacity(height as usize);
    for row in 0..height {
        let inset = row_inset(row, width, height);
        let strip_left = desired.left.saturating_add(inset);
        let strip_right = desired.right.saturating_sub(inset);
        if strip_right <= strip_left {
            continue;
        }

        let strip_top = desired.top.saturating_add(row);
        if let Some(previous) = rectangles.last_mut()
            && previous.left == strip_left
            && previous.right == strip_right
            && previous.bottom == strip_top
        {
            previous.bottom = strip_top.saturating_add(1);
        } else {
            rectangles.push(RectI {
                left: strip_left,
                top: strip_top,
                right: strip_right,
                bottom: strip_top.saturating_add(1),
            });
        }
    }
    rectangles
}

fn create_region_from_rectangles(rectangles: &[RectI]) -> HRGN {
    let Some(first) = rectangles.first().copied() else {
        return HRGN(null_mut());
    };
    let mut bounds = first;
    for rectangle in &rectangles[1..] {
        bounds.left = bounds.left.min(rectangle.left);
        bounds.top = bounds.top.min(rectangle.top);
        bounds.right = bounds.right.max(rectangle.right);
        bounds.bottom = bounds.bottom.max(rectangle.bottom);
    }

    let Some(rectangle_bytes) = size_of::<RECT>().checked_mul(rectangles.len()) else {
        return HRGN(null_mut());
    };
    let Some(data_bytes) = size_of::<RGNDATAHEADER>().checked_add(rectangle_bytes) else {
        return HRGN(null_mut());
    };
    let (Ok(data_bytes_u32), Ok(rectangle_bytes_u32), Ok(rectangle_count)) = (
        u32::try_from(data_bytes),
        u32::try_from(rectangle_bytes),
        u32::try_from(rectangles.len()),
    ) else {
        return HRGN(null_mut());
    };
    let word_size = size_of::<u32>();
    let Some(storage_bytes) = data_bytes.checked_add(word_size - 1) else {
        return HRGN(null_mut());
    };
    let mut storage = vec![0_u32; storage_bytes / word_size];
    let data_ptr = storage.as_mut_ptr().cast::<u8>();
    unsafe {
        // ExtCreateRegion consumes all vertically coalesced scan strips in one GDI call.
        // This preserves the row-exact silhouette without per-row GDI object churn.
        data_ptr.cast::<RGNDATAHEADER>().write(RGNDATAHEADER {
            dwSize: size_of::<RGNDATAHEADER>() as u32,
            iType: RDH_RECTANGLES,
            nCount: rectangle_count,
            nRgnSize: rectangle_bytes_u32,
            rcBound: RECT {
                left: bounds.left,
                top: bounds.top,
                right: bounds.right,
                bottom: bounds.bottom,
            },
        });
        let rectangle_ptr = data_ptr.add(size_of::<RGNDATAHEADER>()).cast::<RECT>();
        for (index, rectangle) in rectangles.iter().enumerate() {
            rectangle_ptr.add(index).write(RECT {
                left: rectangle.left,
                top: rectangle.top,
                right: rectangle.right,
                bottom: rectangle.bottom,
            });
        }
        ExtCreateRegion(None, data_bytes_u32, data_ptr.cast::<RGNDATA>())
    }
}

fn coalesced_region_rectangles(desired: WindowRegion) -> Vec<RectI> {
    match desired.shape {
        WindowRegionShape::Rectangle => {
            if desired.right > desired.left && desired.bottom > desired.top {
                vec![RectI {
                    left: desired.left,
                    top: desired.top,
                    right: desired.right,
                    bottom: desired.bottom,
                }]
            } else {
                Vec::new()
            }
        }
        WindowRegionShape::Island {
            shoulder_start,
            shoulder_depth,
            shoulder_inset,
            bottom_radius,
        } => coalesced_row_rectangles(desired, |row, width, height| {
            island_row_inset(
                row,
                width,
                height,
                shoulder_start,
                shoulder_depth,
                shoulder_inset,
                bottom_radius,
            )
        }),
        WindowRegionShape::RoundedRectangle { corner_radius } => {
            coalesced_row_rectangles(desired, |row, width, height| {
                rounded_rectangle_row_inset(row, width, height, corner_radius)
            })
        }
    }
}

fn apply_window_region(hwnd: HWND, desired: WindowRegion, redraw: bool) -> bool {
    let region = match desired.shape {
        WindowRegionShape::Rectangle => unsafe {
            CreateRectRgn(desired.left, desired.top, desired.right, desired.bottom)
        },
        WindowRegionShape::Island { .. } | WindowRegionShape::RoundedRectangle { .. } => {
            create_region_from_rectangles(&coalesced_region_rectangles(desired))
        }
    };
    if region.0.is_null() {
        return false;
    }
    let applied = unsafe { SetWindowRgn(hwnd, Some(region), redraw) };
    if applied == 0 {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(region.0));
        }
        return false;
    }
    true
}

fn caption_geometry(
    window_rect: RectI,
    visible_frame: RectI,
    caption_relative: Option<RectI>,
    caption_height: i32,
) -> Option<CaptionGeometry> {
    if visible_frame.width() <= 0 || visible_frame.height() < 24 {
        return None;
    }

    // DWMWA_CAPTION_BUTTON_BOUNDS describes only the caption-button area. Its top
    // and height are not the title-bar band and can shrink when a window is
    // maximized. Preserve a healthy bottom anchor so 30px and 46px Teams title
    // bars keep their existing alignment, but make the occupied band at least the
    // product's fixed logical height. Invalid vertical data falls back to the top
    // of the visible DWM frame. The rectangle's X remains the button exclusion.
    let height = caption_height.clamp(24, visible_frame.height());
    let absolute_caption =
        caption_relative.map(|relative| relative.offset(window_rect.left, window_rect.top));
    let minimum_bottom = visible_frame
        .top
        .saturating_add(height)
        .min(visible_frame.bottom);
    let maximum_bottom = visible_frame
        .top
        .saturating_add(height.saturating_mul(2))
        .min(visible_frame.bottom);
    let observed_bottom = absolute_caption.and_then(|absolute| {
        let clipped_top = absolute.top.max(visible_frame.top);
        let clipped_bottom = absolute.bottom.min(visible_frame.bottom);
        (clipped_bottom > clipped_top && clipped_bottom <= maximum_bottom).then_some(clipped_bottom)
    });
    let bottom = observed_bottom
        .unwrap_or(minimum_bottom)
        .max(minimum_bottom);

    let fallback_buttons_left = visible_frame.right - visible_frame.width() / 8;
    let buttons_left = absolute_caption
        .and_then(|absolute| {
            let intersects_frame =
                absolute.right > visible_frame.left && absolute.left < visible_frame.right;
            let left = absolute.left.clamp(visible_frame.left, visible_frame.right);
            (intersects_frame && left > visible_frame.center_x()).then_some(left)
        })
        .unwrap_or(fallback_buttons_left);

    Some(CaptionGeometry {
        band: RectI {
            left: visible_frame.left,
            top: bottom - height,
            right: visible_frame.right,
            bottom,
        },
        buttons_left,
    })
}

fn calculate_placement(
    target_window: RectI,
    target_frame: RectI,
    caption_relative: Option<RectI>,
    overlay: WindowMetrics,
    presentation: OverlayPresentation,
) -> OverlayPlacement {
    let full_surface = surface_rect(
        overlay.client_width,
        overlay.client_height,
        if presentation.is_inline() {
            OverlayMode::Inline
        } else {
            OverlayMode::Expanded
        },
    );
    let compact_surface = compact_surface_rect_for_presentation(
        overlay.client_width,
        overlay.client_height,
        presentation,
    );
    let full_width = full_surface.width();
    let full_height = full_surface.height();
    if full_width <= 0 || full_height <= 0 || target_frame.width() <= 0 {
        return OverlayPlacement::Hidden;
    }

    let island_drop = if presentation.is_inline() {
        0
    } else {
        nominal_island_drop(overlay).clamp(0, full_height)
    };
    // Every presentation reserves one logical pixel above its visible surface for the
    // DWM outer frame. Keep the nominal caption at its full fixed height so bottom
    // anchoring leaves that outline unobscured independently of caption-button Y.
    let caption_height = nominal_caption_height(overlay);
    let Some(caption) = caption_geometry(
        target_window,
        target_frame,
        caption_relative,
        caption_height,
    ) else {
        return OverlayPlacement::Hidden;
    };
    if full_height > caption.band.height().saturating_add(island_drop) {
        return OverlayPlacement::Hidden;
    }

    let safe_left = target_frame.left + target_frame.width() / 6;
    let safe_right = (caption.buttons_left - 8).min(target_frame.right - 8);
    let available_width = safe_right.saturating_sub(safe_left);
    let compact = available_width < full_width;
    let active_surface = if compact {
        compact_surface
    } else {
        full_surface
    };
    let preferred_center = target_frame.center_x();
    let active_width = active_surface.width();
    let left_extent = active_width / 2;
    let right_extent = active_width - left_extent;
    let minimum_center = safe_left.saturating_add(left_extent);
    let maximum_center = safe_right.saturating_sub(right_extent);
    if maximum_center < minimum_center {
        return OverlayPlacement::Hidden;
    }
    let surface_center = preferred_center.clamp(minimum_center, maximum_center);

    let client_offset_x = overlay.client_screen_left - overlay.window_rect.left;
    let client_offset_y = overlay.client_screen_top - overlay.window_rect.top;
    let desired_surface_left = surface_center - active_surface.width() / 2;
    let x = desired_surface_left - client_offset_x - active_surface.left;

    let desired_surface_bottom = caption.band.bottom.saturating_add(island_drop);
    let desired_surface_top = desired_surface_bottom.saturating_sub(full_surface.height());
    let y = desired_surface_top - client_offset_y - full_surface.top;
    let placed_top = y + client_offset_y + full_surface.top;
    let placed_bottom = y + client_offset_y + full_surface.bottom;
    if placed_top < caption.band.top || placed_bottom > desired_surface_bottom {
        return OverlayPlacement::Hidden;
    }

    OverlayPlacement::Visible { x, y, compact }
}

fn calculate_presenter_placement(
    presenter_frame: RectI,
    overlay: WindowMetrics,
    presentation: OverlayPresentation,
) -> OverlayPlacement {
    let full_surface = surface_rect(
        overlay.client_width,
        overlay.client_height,
        if presentation.is_inline() {
            OverlayMode::Inline
        } else {
            OverlayMode::Expanded
        },
    );
    let compact_surface = compact_surface_rect_for_presentation(
        overlay.client_width,
        overlay.client_height,
        presentation,
    );
    if presenter_frame.width() <= 0
        || presenter_frame.height() <= 0
        || full_surface.width() <= 0
        || full_surface.height() <= 0
    {
        return OverlayPlacement::Hidden;
    }

    let compact = presenter_frame.width() < full_surface.width();
    let active_surface = if compact {
        compact_surface
    } else {
        full_surface
    };
    let client_offset_x = overlay.client_screen_left - overlay.window_rect.left;
    let client_offset_y = overlay.client_screen_top - overlay.window_rect.top;
    let desired_surface_left = presenter_frame.center_x() - active_surface.width() / 2;
    let x = desired_surface_left - client_offset_x - active_surface.left;
    // Attach the first visible row directly to the presenter toolbar's lower edge.
    // The overlay window keeps its one-pixel transparent frame above that row, so
    // the two surfaces read as one shape without covering Teams controls.
    let y = presenter_frame.bottom - client_offset_y - active_surface.top;

    OverlayPlacement::Visible { x, y, compact }
}

fn desired_presenter_placement(
    overlay_hwnd: HWND,
    presenter_hwnd: HWND,
    presentation: OverlayPresentation,
) -> OverlayPlacement {
    unsafe {
        if !IsWindow(Some(presenter_hwnd)).as_bool()
            || !IsWindowVisible(presenter_hwnd).as_bool()
            || IsIconic(presenter_hwnd).as_bool()
        {
            return OverlayPlacement::Hidden;
        }
    }

    let Some(presenter_frame) =
        extended_frame_bounds(presenter_hwnd).or_else(|| get_window_rect(presenter_hwnd))
    else {
        return OverlayPlacement::Hidden;
    };
    let Some(overlay) = window_metrics(overlay_hwnd) else {
        return OverlayPlacement::Hidden;
    };
    calculate_presenter_placement(presenter_frame, overlay, presentation)
}

fn desired_placement(
    overlay_hwnd: HWND,
    target_hwnd: HWND,
    presentation: OverlayPresentation,
) -> OverlayPlacement {
    unsafe {
        if !IsWindow(Some(target_hwnd)).as_bool()
            || !IsWindowVisible(target_hwnd).as_bool()
            || IsIconic(target_hwnd).as_bool()
        {
            return OverlayPlacement::Hidden;
        }
    }

    let Some(target_window) = get_window_rect(target_hwnd) else {
        return OverlayPlacement::Hidden;
    };
    let Some(target_frame) = extended_frame_bounds(target_hwnd) else {
        return OverlayPlacement::Hidden;
    };
    let Some(overlay) = window_metrics(overlay_hwnd) else {
        return OverlayPlacement::Hidden;
    };
    calculate_placement(
        target_window,
        target_frame,
        caption_button_bounds(target_hwnd),
        overlay,
        presentation,
    )
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
                    None,
                    x,
                    y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
                );
            }
        }
    }
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

fn sync_overlay_z_order(overlay_hwnd: HWND, target_hwnd: HWND) {
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
        return;
    };
    let insert_after = match anchor {
        OverlayZOrderAnchor::Topmost => HWND_TOPMOST,
        OverlayZOrderAnchor::NotTopmost => HWND_NOTOPMOST,
        OverlayZOrderAnchor::After(window) => HWND(window as *mut c_void),
    };

    unsafe {
        let _ = SetWindowPos(
            overlay_hwnd,
            Some(insert_after),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        );
    }
}

fn get_window_rect(hwnd: HWND) -> Option<RectI> {
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut rect).ok()?;
    }
    (rect.right > rect.left && rect.bottom > rect.top).then_some(rect.into())
}

fn caption_button_bounds(hwnd: HWND) -> Option<RectI> {
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
    (rect.right > rect.left && rect.bottom > rect.top).then_some(rect.into())
}

fn extended_frame_bounds(hwnd: HWND) -> Option<RectI> {
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
    (rect.right > rect.left && rect.bottom > rect.top).then_some(rect.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected_row_inset(region: WindowRegion, row: i32, width: i32, height: i32) -> i32 {
        match region.shape {
            WindowRegionShape::Rectangle => 0,
            WindowRegionShape::Island {
                shoulder_start,
                shoulder_depth,
                shoulder_inset,
                bottom_radius,
            } => island_row_inset(
                row,
                width,
                height,
                shoulder_start,
                shoulder_depth,
                shoulder_inset,
                bottom_radius,
            ),
            WindowRegionShape::RoundedRectangle { corner_radius } => {
                rounded_rectangle_row_inset(row, width, height, corner_radius)
            }
        }
    }

    fn assert_coalesced_region_is_exact(region: WindowRegion) {
        let width = region.right.saturating_sub(region.left);
        let height = region.bottom.saturating_sub(region.top);
        let rectangles = coalesced_region_rectangles(region);
        assert!(width > 0 && height > 0);
        assert!(!rectangles.is_empty());
        assert!(rectangles.len() <= height as usize);

        for rectangle in &rectangles {
            assert!(rectangle.left >= region.left);
            assert!(rectangle.right <= region.right);
            assert!(rectangle.top >= region.top);
            assert!(rectangle.bottom <= region.bottom);
            assert!(rectangle.left < rectangle.right);
            assert!(rectangle.top < rectangle.bottom);
        }
        for pair in rectangles.windows(2) {
            assert_eq!(pair[0].bottom, pair[1].top);
            assert_ne!((pair[0].left, pair[0].right), (pair[1].left, pair[1].right));
        }

        for y in region.top..region.bottom {
            let mut covering = rectangles
                .iter()
                .filter(|rectangle| rectangle.top <= y && y < rectangle.bottom);
            let rectangle = covering.next().expect("every visible row must be covered");
            assert!(covering.next().is_none(), "rows must not overlap");
            let row = y.saturating_sub(region.top);
            let inset = expected_row_inset(region, row, width, height);
            assert_eq!(rectangle.left, region.left.saturating_add(inset));
            assert_eq!(rectangle.right, region.right.saturating_sub(inset));
        }
    }

    #[test]
    fn worker_wakes_coalesce_without_losing_dirty_reasons() {
        let pending_work = AtomicU32::new(0);
        let (wake_tx, wake_rx) = sync_channel(1);

        request_worker_work(&pending_work, &wake_tx, WORK_REGION);
        request_worker_work(&pending_work, &wake_tx, WORK_FOLLOW);

        assert_eq!(wake_rx.try_recv(), Ok(()));
        assert!(wake_rx.try_recv().is_err());
        assert_eq!(
            pending_work.swap(0, Ordering::AcqRel),
            WORK_FOLLOW | WORK_REGION
        );
    }

    #[test]
    fn animation_work_does_not_run_follow_or_z_order_work() {
        assert_eq!(classify_worker_work(WORK_REGION, false), (false, true));
        assert_eq!(classify_worker_work(WORK_FOLLOW, false), (true, true));
        assert_eq!(classify_worker_work(0, true), (true, true));
    }

    #[test]
    fn disclosure_publisher_latches_the_latest_frame_while_wake_is_coalesced() {
        let progress = Arc::new(AtomicU32::new(0));
        let pending_work = Arc::new(AtomicU32::new(0));
        let (wake_tx, wake_rx) = sync_channel(1);
        let publisher = DisclosureProgressPublisher {
            progress: Arc::clone(&progress),
            pending_work: Arc::clone(&pending_work),
            wake_tx,
        };

        for frame in 1..=120 {
            publisher.publish(frame as f32 / 120.0);
        }

        assert_eq!(wake_rx.try_recv(), Ok(()));
        assert!(wake_rx.try_recv().is_err());
        assert_eq!(progress.load(Ordering::Acquire), DISCLOSURE_PROGRESS_MAX);
        assert_eq!(pending_work.load(Ordering::Acquire), WORK_REGION);
    }

    #[test]
    fn colorref_is_converted_from_bgr_to_rgb() {
        assert_eq!(colorref_to_rgb(COLORREF(0x0033_2211)), 0x112233);
    }

    #[test]
    fn representative_color_rejects_isolated_foreground_pixels() {
        let samples = [0xf3f3f3, 0xf3f3f3, 0xf4f4f4, 0xf3f3f3, 0x202020];
        assert_eq!(representative_color(&samples), Some(0xf3f3f3));
    }

    #[test]
    fn tiny_sampling_noise_does_not_trigger_a_palette_update() {
        assert!(!colors_materially_differ(0xf3f3f3, 0xf5f4f3));
        assert!(colors_materially_differ(0xf3f3f3, 0x202020));
    }

    #[test]
    fn titlebar_samples_avoid_the_island_and_caption_buttons() {
        let caption = CaptionGeometry {
            band: RectI {
                left: 0,
                top: 10,
                right: 1_200,
                bottom: 40,
            },
            buttons_left: 1_050,
        };
        let points = titlebar_sample_points(caption);
        assert!(!points.is_empty());
        assert!(points.iter().all(|(x, y)| {
            (*x <= 442 || *x >= 758) && *x < caption.buttons_left && (10..40).contains(y)
        }));
    }

    #[test]
    fn inline_presentation_is_selected_by_either_launch_flag() {
        assert_eq!(
            OverlayPresentation::from_arguments([
                String::from("snapbar.exe"),
                String::from("--inline-titlebar"),
            ]),
            OverlayPresentation::InlineTitlebar
        );
        assert_eq!(
            OverlayPresentation::from_arguments([
                String::from("snapbar.exe"),
                String::from("--inline"),
            ]),
            OverlayPresentation::InlineTitlebar
        );
        assert_eq!(
            OverlayPresentation::from_arguments([String::from("snapbar.exe")]),
            OverlayPresentation::HoverIsland
        );
    }

    #[test]
    fn capture_is_recordable_by_default_but_can_be_explicitly_excluded() {
        assert_eq!(
            OverlayCaptureMode::from_arguments(["snapbar.exe"]),
            OverlayCaptureMode::Recordable
        );
        assert_eq!(
            OverlayCaptureMode::from_arguments(["snapbar.exe", "--recordable-overlay"]),
            OverlayCaptureMode::Recordable
        );
        assert_eq!(
            OverlayCaptureMode::from_arguments([
                "snapbar.exe",
                "--recordable-overlay",
                "--exclude-overlay-from-capture",
            ]),
            OverlayCaptureMode::Excluded
        );
        assert_eq!(
            OverlayCaptureMode::Excluded.display_affinity(),
            WDA_EXCLUDEFROMCAPTURE
        );
        assert_eq!(OverlayCaptureMode::Recordable.display_affinity(), WDA_NONE);
    }

    #[test]
    fn animation_wakes_do_not_postpone_the_full_follow_deadline() {
        let start = Instant::now();
        let interval = Duration::from_millis(100);
        assert!(full_follow_due(None, start, interval, false));
        for elapsed_ms in [1, 8, 16, 33, 67, 99] {
            let wake = start + Duration::from_millis(elapsed_ms);
            assert!(!full_follow_due(Some(start), wake, interval, false));
        }
        assert_eq!(
            remaining_follow_wait(Some(start), start + Duration::from_millis(99), interval,),
            Duration::from_millis(1)
        );
        assert!(full_follow_due(
            Some(start),
            start + interval,
            interval,
            false,
        ));
        assert!(full_follow_due(
            Some(start),
            start + Duration::from_millis(1),
            interval,
            true,
        ));
    }

    #[test]
    fn follow_cadence_preserves_presenter_and_idle_behavior() {
        assert_eq!(follow_interval(1, true), PRESENTER_FOLLOW_INTERVAL);
        assert_eq!(follow_interval(1, false), FOLLOW_INTERVAL);
        assert_eq!(follow_interval(0, false), IDLE_INTERVAL);
    }

    #[test]
    fn animated_region_redraws_only_at_endpoints_or_structural_changes() {
        assert!(region_update_redraw(0, false));
        assert!(!region_update_redraw(1, false));
        assert!(!region_update_redraw(500, false));
        assert!(region_update_redraw(DISCLOSURE_PROGRESS_MAX, false));
        assert!(!region_update_redraw(DISCLOSURE_PROGRESS_LIMIT, false));
        assert!(region_update_redraw(500, true));
    }

    #[test]
    fn titlebar_sampling_waits_until_disclosure_motion_settles() {
        let changed = Instant::now();
        assert!(!disclosure_is_settled(
            true,
            DISCLOSURE_PROGRESS_MAX,
            changed,
            changed,
        ));
        assert!(disclosure_is_settled(
            true,
            DISCLOSURE_PROGRESS_MAX,
            changed,
            changed + COLOR_SAMPLE_SETTLE_INTERVAL,
        ));
        assert!(!disclosure_is_settled(
            true,
            DISCLOSURE_PROGRESS_LIMIT,
            changed,
            changed + COLOR_SAMPLE_SETTLE_INTERVAL,
        ));
        assert!(disclosure_is_settled(
            false,
            0,
            changed,
            changed + COLOR_SAMPLE_SETTLE_INTERVAL,
        ));
    }

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

    #[test]
    fn disclosure_requires_pointer_at_timer_boundaries() {
        let mut machine = DisclosureMachine::default();
        assert!(machine.pointer_enter().start_expand);
        let effects = machine.expand_elapsed(false);
        assert_eq!(effects.expanded_changed, None);
        assert_eq!(machine.phase, DisclosurePhase::Collapsed);

        assert!(machine.pointer_enter().start_expand);
        let effects = machine.expand_elapsed(true);
        assert_eq!(effects.expanded_changed, Some(true));
        assert_eq!(machine.phase, DisclosurePhase::Expanded);
    }

    #[test]
    fn reentry_cancels_pending_collapse() {
        let mut machine = DisclosureMachine {
            phase: DisclosurePhase::Expanded,
        };
        assert!(machine.pointer_leave().start_collapse);
        assert_eq!(machine.phase, DisclosurePhase::CollapsePending);
        let effects = machine.pointer_enter();
        assert!(effects.cancel_collapse);
        assert_eq!(machine.phase, DisclosurePhase::Expanded);
        assert_eq!(machine.collapse_elapsed(true).expanded_changed, None);
    }

    #[test]
    fn leaving_expanded_surface_always_collapses() {
        let mut machine = DisclosureMachine {
            phase: DisclosurePhase::Expanded,
        };
        assert!(machine.pointer_leave().start_collapse);
        let effects = machine.collapse_elapsed(false);
        assert_eq!(effects.expanded_changed, Some(false));
        assert_eq!(machine.phase, DisclosurePhase::Collapsed);
    }

    #[test]
    fn reset_cancels_pending_and_expanded_states() {
        for phase in [
            DisclosurePhase::ExpandPending,
            DisclosurePhase::Expanded,
            DisclosurePhase::CollapsePending,
        ] {
            let mut machine = DisclosureMachine { phase };
            let effects = machine.reset();
            assert!(effects.cancel_expand && effects.cancel_collapse);
            assert_eq!(machine.phase, DisclosurePhase::Collapsed);
        }
    }

    #[test]
    fn late_mouse_leave_after_reset_is_harmless() {
        let mut machine = DisclosureMachine {
            phase: DisclosurePhase::Expanded,
        };
        assert_eq!(machine.reset().expanded_changed, Some(false));
        let late_leave = machine.pointer_leave();
        assert!(late_leave.cancel_expand);
        assert!(!late_leave.start_collapse);
        assert_eq!(late_leave.expanded_changed, None);
        assert_eq!(machine.phase, DisclosurePhase::Collapsed);
    }

    #[test]
    fn regions_match_visible_surfaces() {
        assert_eq!(
            surface_rect(280, 48, OverlayMode::Collapsed),
            RectI {
                left: 94,
                top: 1,
                right: 186,
                bottom: 30,
            }
        );
        assert_eq!(
            surface_rect(280, 48, OverlayMode::Expanded),
            RectI {
                left: 4,
                top: 1,
                right: 276,
                bottom: 46,
            }
        );
        assert_eq!(
            surface_rect(280, 48, OverlayMode::Inline),
            RectI {
                left: 4,
                top: 1,
                right: 276,
                bottom: 30,
            }
        );
        assert_eq!(
            surface_rect(280, 48, OverlayMode::Compact),
            RectI {
                left: 117,
                top: 1,
                right: 163,
                bottom: 30,
            }
        );
    }

    #[test]
    fn hover_regions_follow_the_full_island_silhouette() {
        assert_eq!(
            hover_rect(280, 48, OverlayMode::Collapsed),
            surface_rect(280, 48, OverlayMode::Collapsed)
        );
        assert_eq!(
            hover_rect(280, 48, OverlayMode::Expanded),
            surface_rect(280, 48, OverlayMode::Expanded)
        );
        assert_eq!(
            hover_rect(280, 48, OverlayMode::Compact),
            surface_rect(280, 48, OverlayMode::Compact)
        );
    }

    #[test]
    fn hover_region_scales_with_dpi() {
        assert_eq!(
            hover_rect(420, 72, OverlayMode::Collapsed),
            RectI {
                left: 141,
                top: 1,
                right: 279,
                bottom: 45,
            }
        );
    }

    #[test]
    fn native_region_includes_the_client_origin_offset() {
        let metrics = WindowMetrics {
            window_rect: RectI {
                left: -7,
                top: -7,
                right: 287,
                bottom: 55,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: 280,
            client_height: 48,
        };

        assert_eq!(
            window_region(metrics, OverlayMode::Collapsed),
            WindowRegion {
                left: 101,
                top: 8,
                right: 193,
                bottom: 37,
                shape: WindowRegionShape::Rectangle,
            }
        );
    }

    #[test]
    fn idle_regions_are_rectangular_but_expanded_region_has_curved_shoulders() {
        let metrics = WindowMetrics {
            window_rect: RectI {
                left: 0,
                top: 0,
                right: 280,
                bottom: 48,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: 280,
            client_height: 48,
        };

        for mode in [OverlayMode::Collapsed, OverlayMode::Compact] {
            let region = window_region(metrics, mode);
            assert_eq!(region.shape, WindowRegionShape::Rectangle);
        }
        let expanded = window_region(metrics, OverlayMode::Expanded);
        assert_eq!(
            expanded.shape,
            WindowRegionShape::Island {
                shoulder_start: 29,
                shoulder_depth: 10,
                shoulder_inset: 16,
                bottom_radius: 8,
            }
        );
    }

    #[test]
    fn disclosure_geometry_grows_from_the_center_without_moving_the_top_anchor() {
        assert_eq!(disclosure_width(0.0), COLLAPSED_WIDTH);
        assert_eq!(disclosure_height(0.0), TITLEBAR_SURFACE_HEIGHT);
        assert_eq!(disclosure_width(1.0), EXPANDED_WIDTH);
        assert_eq!(disclosure_height(1.0), HOVER_ISLAND_HEIGHT);

        let halfway = disclosure_surface_rect(280, 48, 500);
        assert_eq!(
            halfway,
            RectI {
                left: 49,
                top: 1,
                right: 231,
                bottom: 38,
            }
        );
        assert_eq!(halfway.center_x(), 140);
        for progress in [0, 250, 500, 750, DISCLOSURE_PROGRESS_MAX] {
            assert_eq!(disclosure_surface_rect(280, 48, progress).top, 1);
            assert_eq!(disclosure_surface_rect(280, 48, progress).center_x(), 140);
        }
    }

    #[test]
    fn inline_disclosure_only_grows_horizontally_inside_the_caption_band() {
        for progress in [0, 250, 500, 750, DISCLOSURE_PROGRESS_MAX] {
            let surface = disclosure_surface_rect_for_presentation(
                280,
                48,
                progress,
                OverlayPresentation::InlineTitlebar,
            );
            assert_eq!(surface.top, 1);
            assert_eq!(surface.bottom, 30);
            assert_eq!(surface.height(), 29);
            assert_eq!(surface.center_x(), 140);
        }

        assert_eq!(
            disclosure_surface_rect_for_presentation(
                280,
                48,
                DISCLOSURE_PROGRESS_MAX,
                OverlayPresentation::InlineTitlebar,
            )
            .width(),
            INLINE_WIDTH as i32,
        );
    }

    #[test]
    fn inline_native_region_is_rectangular_and_caption_contained() {
        let metrics = WindowMetrics {
            window_rect: RectI {
                left: 0,
                top: 0,
                right: 280,
                bottom: 48,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: 280,
            client_height: 48,
        };

        assert_eq!(
            window_region(metrics, OverlayMode::Inline),
            WindowRegion {
                left: 4,
                top: 1,
                right: 276,
                bottom: 30,
                shape: WindowRegionShape::Rectangle,
            }
        );
    }

    #[test]
    fn inline_compact_region_reserves_the_same_top_frame_inset() {
        let metrics = WindowMetrics {
            window_rect: RectI {
                left: 0,
                top: 0,
                right: 280,
                bottom: 48,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: 280,
            client_height: 48,
        };

        assert_eq!(
            window_region_for_progress(metrics, OverlayPresentation::InlineTitlebar, true, 0),
            WindowRegion {
                left: 117,
                top: 1,
                right: 163,
                bottom: 30,
                shape: WindowRegionShape::Rectangle,
            }
        );
    }

    #[test]
    fn spring_overshoot_bulges_inside_the_fixed_overlay_envelope() {
        assert_eq!(
            disclosure_surface_rect(280, 48, DISCLOSURE_PROGRESS_LIMIT),
            RectI {
                left: 0,
                top: 1,
                right: 280,
                bottom: 46,
            }
        );
    }

    #[test]
    fn disclosure_region_scales_shoulders_with_dpi() {
        let metrics = WindowMetrics {
            window_rect: RectI {
                left: 0,
                top: 0,
                right: 420,
                bottom: 72,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: 420,
            client_height: 72,
        };

        assert_eq!(
            window_region(metrics, OverlayMode::Expanded),
            WindowRegion {
                left: 6,
                top: 1,
                right: 414,
                bottom: 69,
                shape: WindowRegionShape::Island {
                    shoulder_start: 44,
                    shoulder_depth: 15,
                    shoulder_inset: 24,
                    bottom_radius: 12,
                },
            }
        );
    }

    #[test]
    fn compressed_row_runs_preserve_every_island_scanline() {
        let desired = WindowRegion {
            left: 4,
            top: 1,
            right: 276,
            bottom: 46,
            shape: WindowRegionShape::Island {
                shoulder_start: 29,
                shoulder_depth: 10,
                shoulder_inset: 16,
                bottom_radius: 8,
            },
        };
        let runs = coalesced_row_rectangles(desired, |row, width, height| {
            island_row_inset(row, width, height, 29, 10, 16, 8)
        });
        let run_count = runs.len();

        assert!(run_count < desired.bottom.saturating_sub(desired.top) as usize / 2);
        for row in 0..desired.bottom.saturating_sub(desired.top) {
            let expected_inset = island_row_inset(row, 272, 45, 29, 10, 16, 8);
            let y = desired.top + row;
            let run = runs
                .iter()
                .find(|run| run.top <= y && y < run.bottom)
                .expect("every scanline must be represented");
            assert_eq!(run.left, desired.left + expected_inset);
            assert_eq!(run.right, desired.right - expected_inset);
        }
    }

    #[test]
    fn island_rows_curve_in_at_the_root_and_round_out_at_the_bottom() {
        assert_eq!(island_row_inset(0, 272, 45, 29, 10, 16, 8), 0);
        assert_eq!(island_row_inset(28, 272, 45, 29, 10, 16, 8), 0);
        assert_eq!(island_row_inset(29, 272, 45, 29, 10, 16, 8), 0);
        assert_eq!(island_row_inset(30, 272, 45, 29, 10, 16, 8), 7);
        assert_eq!(island_row_inset(34, 272, 45, 29, 10, 16, 8), 14);
        assert_eq!(island_row_inset(38, 272, 45, 29, 10, 16, 8), 16);
        assert_eq!(island_row_inset(40, 272, 45, 29, 10, 16, 8), 17);
        assert_eq!(island_row_inset(42, 272, 45, 29, 10, 16, 8), 18);
        assert_eq!(island_row_inset(44, 272, 45, 29, 10, 16, 8), 24);
    }

    #[test]
    fn batched_regions_preserve_every_animated_row_at_common_and_fractional_dpis() {
        for (client_width, client_height) in [
            (280, 48),
            (321, 55),
            (338, 58),
            (350, 60),
            (420, 72),
            (490, 84),
            (560, 96),
        ] {
            let metrics = WindowMetrics {
                window_rect: RectI {
                    left: 0,
                    top: 0,
                    right: client_width,
                    bottom: client_height,
                },
                client_screen_left: 0,
                client_screen_top: 0,
                client_width,
                client_height,
            };

            for presenter_attached in [false, true] {
                for progress in 0..=DISCLOSURE_PROGRESS_LIMIT {
                    assert_coalesced_region_is_exact(window_region_for_attachment(
                        metrics,
                        OverlayPresentation::HoverIsland,
                        false,
                        presenter_attached,
                        progress,
                    ));
                }
            }
        }
    }

    #[test]
    fn batched_rectangle_data_creates_a_native_region() {
        let metrics = WindowMetrics {
            window_rect: RectI {
                left: 0,
                top: 0,
                right: 280,
                bottom: 48,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: 280,
            client_height: 48,
        };
        let region = window_region_for_attachment(
            metrics,
            OverlayPresentation::HoverIsland,
            false,
            false,
            DISCLOSURE_PROGRESS_MAX,
        );
        let rectangles = coalesced_region_rectangles(region);
        assert!(rectangles.len() < region.bottom.saturating_sub(region.top) as usize);
        let native = create_region_from_rectangles(&rectangles);
        assert!(!native.0.is_null());
        unsafe {
            assert!(DeleteObject(HGDIOBJ(native.0)).as_bool());
        }
    }

    #[test]
    fn presenter_region_uses_a_plain_rounded_rectangle_instead_of_the_titlebar_shape() {
        let metrics = WindowMetrics {
            window_rect: RectI {
                left: 0,
                top: 0,
                right: 280,
                bottom: 48,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: 280,
            client_height: 48,
        };

        assert_eq!(
            window_region_for_attachment(metrics, OverlayPresentation::HoverIsland, false, true, 0,),
            WindowRegion {
                left: 94,
                top: 1,
                right: 186,
                bottom: 40,
                shape: WindowRegionShape::RoundedRectangle { corner_radius: 16 },
            }
        );
        assert_eq!(
            window_region_for_attachment(
                metrics,
                OverlayPresentation::HoverIsland,
                false,
                true,
                DISCLOSURE_PROGRESS_MAX,
            ),
            WindowRegion {
                left: 4,
                top: 1,
                right: 276,
                bottom: 46,
                shape: WindowRegionShape::RoundedRectangle { corner_radius: 16 },
            }
        );
    }

    #[test]
    fn presenter_compact_region_keeps_the_same_rounded_rectangle_language() {
        let metrics = WindowMetrics {
            window_rect: RectI {
                left: 0,
                top: 0,
                right: 280,
                bottom: 48,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: 280,
            client_height: 48,
        };

        assert_eq!(
            window_region_for_attachment(
                metrics,
                OverlayPresentation::InlineTitlebar,
                true,
                true,
                0,
            ),
            WindowRegion {
                left: 117,
                top: 1,
                right: 163,
                bottom: 40,
                shape: WindowRegionShape::RoundedRectangle { corner_radius: 16 },
            }
        );
    }

    #[test]
    fn presenter_rounded_rectangle_rows_round_all_four_corners_evenly() {
        assert_eq!(rounded_rectangle_row_inset(0, 272, 45, 16), 16);
        assert_eq!(rounded_rectangle_row_inset(1, 272, 45, 16), 10);
        assert_eq!(rounded_rectangle_row_inset(2, 272, 45, 16), 8);
        assert_eq!(rounded_rectangle_row_inset(3, 272, 45, 16), 6);
        assert_eq!(rounded_rectangle_row_inset(5, 272, 45, 16), 4);
        assert_eq!(rounded_rectangle_row_inset(15, 272, 45, 16), 0);
        assert_eq!(rounded_rectangle_row_inset(22, 272, 45, 16), 0);
        assert_eq!(rounded_rectangle_row_inset(41, 272, 45, 16), 6);
        assert_eq!(rounded_rectangle_row_inset(42, 272, 45, 16), 8);
        assert_eq!(rounded_rectangle_row_inset(43, 272, 45, 16), 10);
        assert_eq!(rounded_rectangle_row_inset(44, 272, 45, 16), 16);
    }

    #[test]
    fn native_region_retry_key_changes_with_the_client_origin() {
        let first = WindowMetrics {
            window_rect: RectI {
                left: -7,
                top: -7,
                right: 287,
                bottom: 55,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: 280,
            client_height: 48,
        };
        let shifted = WindowMetrics {
            client_screen_left: 1,
            ..first
        };

        assert_ne!(
            window_region(first, OverlayMode::Collapsed),
            window_region(shifted, OverlayMode::Collapsed)
        );
    }

    #[test]
    fn caption_button_x_is_converted_without_using_its_vertical_extent() {
        let window = RectI {
            left: -7,
            top: -7,
            right: 1927,
            bottom: 1147,
        };
        let frame = RectI {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1140,
        };
        let caption = RectI {
            left: 1684,
            top: 7,
            right: 1927,
            bottom: 53,
        };
        let geometry = caption_geometry(window, frame, Some(caption), 42).unwrap();
        assert_eq!(geometry.band.top, 4);
        assert_eq!(geometry.band.bottom, 46);
        assert_eq!(geometry.buttons_left, 1677);

        let short_maximized_buttons = RectI { top: 25, ..caption };
        assert_eq!(
            caption_geometry(window, frame, Some(short_maximized_buttons), 42),
            Some(geometry)
        );

        let clipped_bottom = RectI {
            top: 7,
            bottom: 35,
            ..caption
        };
        let promoted = caption_geometry(window, frame, Some(clipped_bottom), 42).unwrap();
        assert_eq!(promoted.band.top, 0);
        assert_eq!(promoted.band.bottom, 42);
        assert_eq!(promoted.buttons_left, geometry.buttons_left);

        let implausibly_deep = RectI {
            top: 7,
            bottom: 150,
            ..caption
        };
        let fallback = caption_geometry(window, frame, Some(implausibly_deep), 42).unwrap();
        assert_eq!(fallback.band.top, 0);
        assert_eq!(fallback.band.bottom, 42);

        let invalid_horizontal_bounds = RectI {
            left: 100,
            right: 300,
            ..caption
        };
        let horizontal_fallback =
            caption_geometry(window, frame, Some(invalid_horizontal_bounds), 42).unwrap();
        assert_eq!(horizontal_fallback.band, geometry.band);
        assert_eq!(horizontal_fallback.buttons_left, 1_680);
    }

    #[test]
    fn expanded_surface_preserves_a_healthy_observed_caption_bottom_edge() {
        let overlay = WindowMetrics {
            window_rect: RectI {
                left: 0,
                top: 0,
                right: 350,
                bottom: 50,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: 350,
            client_height: 50,
        };
        let placement = calculate_placement(
            RectI {
                left: -7,
                top: -7,
                right: 1927,
                bottom: 1147,
            },
            RectI {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1140,
            },
            Some(RectI {
                left: 1684,
                top: 7,
                right: 1927,
                bottom: 53,
            }),
            overlay,
            OverlayPresentation::HoverIsland,
        );
        let OverlayPlacement::Visible { y, compact, .. } = placement else {
            panic!("expected visible placement");
        };
        assert!(!compact);
        let expanded = surface_rect(350, 50, OverlayMode::Expanded);
        assert!(y + expanded.top >= 0);
        // Preserve the healthy observed caption bottom (46) and add the scaled drop.
        assert_eq!(y + expanded.bottom, 63);
    }

    #[test]
    fn inline_surface_ends_at_the_caption_bottom_without_a_drop() {
        let overlay = WindowMetrics {
            window_rect: RectI {
                left: 812,
                top: 516,
                right: 1108,
                bottom: 572,
            },
            client_screen_left: 820,
            client_screen_top: 516,
            client_width: 280,
            client_height: 48,
        };
        let placement = calculate_placement(
            RectI {
                left: 299,
                top: 20,
                right: 1619,
                bottom: 845,
            },
            RectI {
                left: 306,
                top: 20,
                right: 1612,
                bottom: 838,
            },
            Some(RectI {
                left: 1167,
                top: 0,
                right: 1313,
                bottom: 30,
            }),
            overlay,
            OverlayPresentation::InlineTitlebar,
        );

        let OverlayPlacement::Visible { y, compact, .. } = placement else {
            panic!("expected visible placement");
        };
        assert!(!compact);
        let inline = surface_rect(280, 48, OverlayMode::Inline);
        // Preserve the one-logical-pixel DWM outline at the top of the 30px caption.
        assert_eq!(y + inline.top, 21);
        assert_eq!(y + inline.bottom, 50);
    }

    #[test]
    fn inline_fallback_caption_also_preserves_the_top_frame_outline() {
        let overlay = WindowMetrics {
            window_rect: RectI {
                left: 812,
                top: 516,
                right: 1108,
                bottom: 572,
            },
            client_screen_left: 820,
            client_screen_top: 516,
            client_width: 280,
            client_height: 48,
        };
        let placement = calculate_placement(
            RectI {
                left: 299,
                top: 20,
                right: 1619,
                bottom: 845,
            },
            RectI {
                left: 306,
                top: 20,
                right: 1612,
                bottom: 838,
            },
            None,
            overlay,
            OverlayPresentation::InlineTitlebar,
        );

        let OverlayPlacement::Visible { y, compact, .. } = placement else {
            panic!("expected visible placement");
        };
        assert!(!compact);
        let inline = surface_rect(280, 48, OverlayMode::Inline);
        assert_eq!(y + inline.top, 21);
        assert_eq!(y + inline.bottom, 50);
    }

    #[test]
    fn hover_island_fallback_caption_also_preserves_the_top_frame_outline() {
        let overlay = WindowMetrics {
            window_rect: RectI {
                left: 812,
                top: 516,
                right: 1108,
                bottom: 572,
            },
            client_screen_left: 820,
            client_screen_top: 516,
            client_width: 280,
            client_height: 48,
        };
        let placement = calculate_placement(
            RectI {
                left: 299,
                top: 20,
                right: 1619,
                bottom: 845,
            },
            RectI {
                left: 306,
                top: 20,
                right: 1612,
                bottom: 838,
            },
            None,
            overlay,
            OverlayPresentation::HoverIsland,
        );

        let OverlayPlacement::Visible { y, compact, .. } = placement else {
            panic!("expected visible placement");
        };
        assert!(!compact);
        let collapsed = surface_rect(280, 48, OverlayMode::Collapsed);
        let expanded = surface_rect(280, 48, OverlayMode::Expanded);
        assert_eq!(y + collapsed.top, 21);
        assert_eq!(y + collapsed.bottom, 50);
        assert_eq!(y + expanded.top, 21);
        assert_eq!(y + expanded.bottom, 66);
    }

    #[test]
    fn hover_island_preserves_the_top_frame_outline_on_a_thirty_pixel_caption() {
        let overlay = WindowMetrics {
            window_rect: RectI {
                left: 812,
                top: 516,
                right: 1108,
                bottom: 572,
            },
            client_screen_left: 820,
            client_screen_top: 516,
            client_width: 280,
            client_height: 48,
        };
        let placement = calculate_placement(
            RectI {
                left: 299,
                top: 20,
                right: 1619,
                bottom: 845,
            },
            RectI {
                left: 306,
                top: 20,
                right: 1612,
                bottom: 838,
            },
            Some(RectI {
                left: 1167,
                top: 0,
                right: 1313,
                bottom: 30,
            }),
            overlay,
            OverlayPresentation::HoverIsland,
        );

        assert_eq!(
            placement,
            OverlayPlacement::Visible {
                x: 811,
                y: 20,
                compact: false,
            }
        );

        let OverlayPlacement::Visible { y, .. } = placement else {
            unreachable!();
        };
        let collapsed = surface_rect(280, 48, OverlayMode::Collapsed);
        let expanded = surface_rect(280, 48, OverlayMode::Expanded);
        assert_eq!(y + collapsed.top, 21);
        assert_eq!(y + collapsed.bottom, 50);
        assert_eq!(y + expanded.top, 21);
        assert_eq!(y + expanded.bottom, 66);
    }

    #[test]
    fn compact_surface_is_clamped_inside_caption_safe_span() {
        let overlay = WindowMetrics {
            window_rect: RectI {
                left: 0,
                top: 0,
                right: 280,
                bottom: 48,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: 280,
            client_height: 48,
        };
        let placement = calculate_placement(
            RectI {
                left: 0,
                top: 0,
                right: 500,
                bottom: 100,
            },
            RectI {
                left: 0,
                top: 0,
                right: 500,
                bottom: 100,
            },
            Some(RectI {
                left: 260,
                top: 0,
                right: 400,
                bottom: 46,
            }),
            overlay,
            OverlayPresentation::HoverIsland,
        );

        assert_eq!(
            placement,
            OverlayPlacement::Visible {
                x: 89,
                y: 16,
                compact: true,
            }
        );
    }

    #[test]
    fn short_maximized_caption_buttons_do_not_hide_at_common_or_custom_dpis() {
        for (client_width, client_height) in [
            (280, 48),
            (321, 55),
            (338, 58),
            (350, 60),
            (420, 72),
            (490, 84),
            (560, 96),
        ] {
            let overlay = WindowMetrics {
                window_rect: RectI {
                    left: 0,
                    top: 0,
                    right: client_width,
                    bottom: client_height,
                },
                client_screen_left: 0,
                client_screen_top: 0,
                client_width,
                client_height,
            };
            let target_window = RectI {
                left: -8,
                top: -8,
                right: 1_928,
                bottom: 1_088,
            };
            let target_frame = RectI {
                left: 0,
                top: 0,
                right: 1_920,
                bottom: 1_080,
            };
            let caption_buttons = RectI {
                left: 1_608,
                top: 8,
                right: 1_928,
                bottom: 36,
            };

            let placement = calculate_placement(
                target_window,
                target_frame,
                Some(caption_buttons),
                overlay,
                OverlayPresentation::HoverIsland,
            );
            let OverlayPlacement::Visible { y, compact, .. } = placement else {
                panic!("expected visible placement at {client_width}x{client_height}");
            };
            assert!(!compact);

            let expanded = surface_rect(client_width, client_height, OverlayMode::Expanded);
            let independently_scaled_drop =
                scale_logical(ISLAND_DROP, client_height, WINDOW_HEIGHT);
            assert!(expanded.height() > 28 + independently_scaled_drop);
            let caption_height = nominal_caption_height(overlay);
            let drop = nominal_island_drop(overlay);
            assert!(y + expanded.top > target_frame.top);
            assert_eq!(
                y + expanded.bottom,
                target_frame.top + caption_height + drop
            );
        }
    }

    #[test]
    fn compact_surface_hides_when_caption_safe_span_is_too_narrow() {
        let overlay = WindowMetrics {
            window_rect: RectI {
                left: 0,
                top: 0,
                right: 280,
                bottom: 48,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: 280,
            client_height: 48,
        };
        let placement = calculate_placement(
            RectI {
                left: 0,
                top: 0,
                right: 100,
                bottom: 100,
            },
            RectI {
                left: 0,
                top: 0,
                right: 100,
                bottom: 100,
            },
            Some(RectI {
                left: 51,
                top: 0,
                right: 100,
                bottom: 46,
            }),
            overlay,
            OverlayPresentation::HoverIsland,
        );

        assert_eq!(placement, OverlayPlacement::Hidden);
    }

    #[test]
    fn presenter_overlay_attaches_to_the_toolbar_bottom_center() {
        let overlay = WindowMetrics {
            window_rect: RectI {
                left: 0,
                top: 0,
                right: 280,
                bottom: 48,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: 280,
            client_height: 48,
        };
        let placement = calculate_presenter_placement(
            RectI {
                left: 812,
                top: 8,
                right: 1_109,
                bottom: 43,
            },
            overlay,
            OverlayPresentation::HoverIsland,
        );

        assert_eq!(
            placement,
            OverlayPlacement::Visible {
                x: 820,
                y: 42,
                compact: false,
            }
        );
        let OverlayPlacement::Visible { x, y, .. } = placement else {
            unreachable!();
        };
        let expanded = surface_rect(280, 48, OverlayMode::Expanded);
        assert_eq!(x + expanded.left + expanded.width() / 2, 960);
        assert_eq!(y + expanded.top, 43);
    }

    #[test]
    fn presenter_overlay_tracks_toolbar_expansion_without_a_second_anchor() {
        let overlay = WindowMetrics {
            window_rect: RectI {
                left: 0,
                top: 0,
                right: 280,
                bottom: 48,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: 280,
            client_height: 48,
        };
        let placement = calculate_presenter_placement(
            RectI {
                left: 566,
                top: 8,
                right: 1_356,
                bottom: 63,
            },
            overlay,
            OverlayPresentation::HoverIsland,
        );

        assert_eq!(
            placement,
            OverlayPlacement::Visible {
                x: 821,
                y: 62,
                compact: false,
            }
        );
    }

    #[test]
    fn presenter_overlay_supports_negative_monitor_coordinates() {
        let overlay = WindowMetrics {
            window_rect: RectI {
                left: 0,
                top: 0,
                right: 280,
                bottom: 48,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: 280,
            client_height: 48,
        };
        let placement = calculate_presenter_placement(
            RectI {
                left: -1_500,
                top: -1_430,
                right: -710,
                bottom: -1_375,
            },
            overlay,
            OverlayPresentation::HoverIsland,
        );

        let OverlayPlacement::Visible { x, y, compact } = placement else {
            panic!("expected visible placement");
        };
        assert!(!compact);
        assert!(x < 0);
        assert!(y < 0);
    }

    #[test]
    fn negative_monitor_coordinates_remain_valid() {
        let overlay = WindowMetrics {
            window_rect: RectI {
                left: 0,
                top: 0,
                right: 280,
                bottom: 48,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: 280,
            client_height: 48,
        };
        let placement = calculate_placement(
            RectI {
                left: 821,
                top: -1455,
                right: 3090,
                bottom: -44,
            },
            RectI {
                left: 828,
                top: -1448,
                right: 3083,
                bottom: -51,
            },
            Some(RectI {
                left: 1990,
                top: 7,
                right: 2269,
                bottom: 53,
            }),
            overlay,
            OverlayPresentation::HoverIsland,
        );
        assert!(matches!(placement, OverlayPlacement::Visible { .. }));
    }
}
