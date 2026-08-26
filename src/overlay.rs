use std::{
    ffi::c_void,
    ptr::null_mut,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{RecvTimeoutError, SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use async_channel::{Receiver, Sender};
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
const EXPAND_DELAY_MS: u32 = 180;
const COLLAPSE_DELAY_MS: u32 = 400;
const SAFETY_INTERVAL_MS: u32 = 250;
const DWMWA_COLOR_NONE: u32 = 0xffff_fffe;
const SUBCLASS_ID: usize = 0x534e_4150;
const TIMER_EXPAND: usize = 0x534e_01;
const TIMER_COLLAPSE: usize = 0x534e_02;
const TIMER_SAFETY: usize = 0x534e_03;
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
const TME_CANCEL: u32 = 0x8000_0000;
const MA_NOACTIVATE: isize = 3;

pub const WINDOW_WIDTH: f32 = 280.0;
pub const WINDOW_HEIGHT: f32 = 40.0;
const COLLAPSED_WIDTH: f32 = 92.0;
const COLLAPSED_HEIGHT: f32 = 28.0;
const EXPANDED_WIDTH: f32 = 272.0;
const EXPANDED_HEIGHT: f32 = 34.0;
const COMPACT_WIDTH: f32 = 40.0;
const COMPACT_HEIGHT: f32 = 34.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayMode {
    Collapsed,
    Expanded,
    Compact,
}

impl OverlayMode {
    pub const fn from_state(expanded: bool, compact: bool) -> Self {
        if compact {
            Self::Compact
        } else if expanded {
            Self::Expanded
        } else {
            Self::Collapsed
        }
    }

    pub const fn logical_width(self) -> f32 {
        match self {
            Self::Collapsed => COLLAPSED_WIDTH,
            Self::Expanded => EXPANDED_WIDTH,
            Self::Compact => COMPACT_WIDTH,
        }
    }

    pub const fn logical_height(self) -> f32 {
        match self {
            Self::Collapsed => COLLAPSED_HEIGHT,
            Self::Expanded => EXPANDED_HEIGHT,
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
    wake_tx: SyncSender<()>,
    event_tx: Sender<()>,
}

impl HoverSubclassState {
    fn interactive(&self) -> bool {
        self.visible.load(Ordering::Acquire) && !self.compact.load(Ordering::Acquire)
    }

    fn publish(&self) {
        let _ = self.wake_tx.try_send(());
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

    fn cancel_leave_tracking(&mut self, hwnd: *mut c_void) {
        if !self.tracking_leave {
            return;
        }
        let mut event = TrackMouseEventRaw {
            cb_size: std::mem::size_of::<TrackMouseEventRaw>() as u32,
            flags: TME_CANCEL | TME_LEAVE,
            hwnd_track: hwnd,
            hover_time: 0,
        };
        unsafe {
            let _ = TrackMouseEvent(&mut event);
        }
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
        self.cancel_leave_tracking(hwnd);
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
            wake_tx,
            event_tx,
        });
        let state_ptr = Box::into_raw(state) as usize;
        let installed = unsafe {
            SetWindowSubclass(
                hwnd.0,
                Some(overlay_subclass_proc),
                SUBCLASS_ID,
                state_ptr,
            )
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
                let effects = state.machine.expand_elapsed(pointer_over && state.interactive());
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

#[derive(Clone, Copy, Debug)]
struct WindowMetrics {
    window_rect: RectI,
    client_screen_left: i32,
    client_screen_top: i32,
    client_width: i32,
    client_height: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CaptionGeometry {
    band: RectI,
    buttons_left: i32,
}

pub struct TeamsWindowFollower {
    target_id: Arc<AtomicU32>,
    expanded: Arc<AtomicBool>,
    compact: Arc<AtomicBool>,
    visible: Arc<AtomicBool>,
    event_rx: Receiver<()>,
    wake_tx: SyncSender<()>,
    native_hover: Option<NativeHoverSubclass>,
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
        let (event_tx, event_rx) = async_channel::bounded(1);
        let native_hover = NativeHoverSubclass::install(
            overlay_hwnd,
            Arc::clone(&expanded),
            Arc::clone(&compact),
            Arc::clone(&visible),
            wake_tx.clone(),
            event_tx.clone(),
        )?;

        let thread_target_id = Arc::clone(&target_id);
        let thread_expanded = Arc::clone(&expanded);
        let thread_compact = Arc::clone(&compact);
        let thread_visible = Arc::clone(&visible);
        let thread_stop = Arc::clone(&stop);
        let thread_event_tx = event_tx.clone();
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
                let mut previous_visible = false;
                let mut previous_compact = false;
                while !thread_stop.load(Ordering::Acquire) {
                    let target_id = thread_target_id.load(Ordering::Acquire);
                    let placement = if target_id == 0 {
                        OverlayPlacement::Hidden
                    } else {
                        let target_hwnd = HWND(target_id as usize as *mut c_void);
                        desired_placement(overlay_hwnd, target_hwnd)
                    };

                    let visible_now = matches!(placement, OverlayPlacement::Visible { .. });
                    let compact_now =
                        matches!(placement, OverlayPlacement::Visible { compact: true, .. });
                    thread_visible.store(visible_now, Ordering::Release);
                    thread_compact.store(compact_now, Ordering::Release);

                    let visibility_changed = visible_now != previous_visible;
                    let compact_changed = compact_now != previous_compact;
                    if visibility_changed || compact_changed {
                        if !visible_now || compact_now {
                            thread_expanded.store(false, Ordering::Release);
                            post_overlay_message(overlay_hwnd, WM_APP_RESET_DISCLOSURE);
                        }
                        let _ = thread_event_tx.try_send(());
                        previous_visible = visible_now;
                        previous_compact = compact_now;
                    }

                    let mode = OverlayMode::from_state(
                        thread_expanded.load(Ordering::Acquire),
                        compact_now,
                    );
                    let region_state = (client_size(overlay_hwnd), mode);
                    if previous_region_state != Some(region_state) {
                        apply_window_region(overlay_hwnd, mode);
                        previous_region_state = Some(region_state);
                    }

                    if previous_placement != Some(placement) {
                        apply_placement(overlay_hwnd, placement);
                        if visible_now {
                            post_overlay_message(overlay_hwnd, WM_APP_REEVALUATE_POINTER);
                        }
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
                thread_expanded.store(false, Ordering::Release);
                let _ = thread_event_tx.try_send(());
                apply_placement(overlay_hwnd, OverlayPlacement::Hidden);
            })
            .ok()?;

        Some(Self {
            target_id,
            expanded,
            compact,
            visible,
            event_rx,
            wake_tx,
            native_hover: Some(native_hover),
            stop,
            worker: Some(worker),
        })
    }

    fn wake(&self) {
        let _ = self.wake_tx.try_send(());
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
            self.wake();
        }
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded.load(Ordering::Acquire)
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

fn client_size(hwnd: HWND) -> Option<(i32, i32)> {
    let metrics = window_metrics(hwnd)?;
    Some((metrics.client_width, metrics.client_height))
}

fn scale_logical(value: f32, actual: i32, logical: f32) -> i32 {
    ((value * actual as f32) / logical).round() as i32
}

fn surface_rect(width: i32, height: i32, mode: OverlayMode) -> RectI {
    let surface_width = scale_logical(mode.logical_width(), width, WINDOW_WIDTH).clamp(1, width);
    let surface_height =
        scale_logical(mode.logical_height(), height, WINDOW_HEIGHT).clamp(1, height);
    let surface_left = (width - surface_width) / 2;
    let surface_top = (height - surface_height) / 2;

    RectI {
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

    let client_left = metrics.client_screen_left - metrics.window_rect.left;
    let client_top = metrics.client_screen_top - metrics.window_rect.top;
    let surface = surface_rect(metrics.client_width, metrics.client_height, mode);
    let height = surface.height().max(1);
    let region = unsafe {
        CreateRoundRectRgn(
            client_left + surface.left,
            client_top + surface.top,
            client_left + surface.right,
            client_top + surface.bottom,
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

fn caption_geometry(
    window_rect: RectI,
    visible_frame: RectI,
    caption_relative: Option<RectI>,
    fallback_height: i32,
) -> Option<CaptionGeometry> {
    if let Some(relative) = caption_relative {
        let absolute = relative.offset(window_rect.left, window_rect.top);
        let band = RectI {
            left: visible_frame.left,
            top: absolute.top.max(visible_frame.top),
            right: visible_frame.right,
            bottom: absolute.bottom.min(visible_frame.bottom),
        };
        let buttons_left = absolute.left.clamp(visible_frame.left, visible_frame.right);
        if (24..=96).contains(&band.height()) && buttons_left > visible_frame.center_x() {
            return Some(CaptionGeometry { band, buttons_left });
        }
    }

    let maximum_height = visible_frame.height().min(96);
    if maximum_height < 24 {
        return None;
    }
    let height = fallback_height.clamp(24, maximum_height);
    Some(CaptionGeometry {
        band: RectI {
            left: visible_frame.left,
            top: visible_frame.top,
            right: visible_frame.right,
            bottom: visible_frame.top + height,
        },
        buttons_left: visible_frame.right - visible_frame.width() / 8,
    })
}

fn calculate_placement(
    target_window: RectI,
    target_frame: RectI,
    caption_relative: Option<RectI>,
    overlay: WindowMetrics,
) -> OverlayPlacement {
    let expanded_surface = surface_rect(
        overlay.client_width,
        overlay.client_height,
        OverlayMode::Expanded,
    );
    let compact_surface = surface_rect(
        overlay.client_width,
        overlay.client_height,
        OverlayMode::Compact,
    );
    let expanded_width = expanded_surface.width();
    let expanded_height = expanded_surface.height();
    if expanded_width <= 0 || expanded_height <= 0 || target_frame.width() <= 0 {
        return OverlayPlacement::Hidden;
    }

    let Some(caption) = caption_geometry(
        target_window,
        target_frame,
        caption_relative,
        expanded_height + 8,
    ) else {
        return OverlayPlacement::Hidden;
    };
    if expanded_height > caption.band.height() {
        return OverlayPlacement::Hidden;
    }

    let safe_left = target_frame.left + target_frame.width() / 6;
    let safe_right = (caption.buttons_left - 8).min(target_frame.right - 8);
    let available_width = safe_right.saturating_sub(safe_left);
    let compact = available_width < expanded_width;
    let active_surface = if compact {
        compact_surface
    } else {
        expanded_surface
    };
    let preferred_center = target_frame.center_x();
    let surface_center = if compact {
        preferred_center
    } else {
        let minimum_center = safe_left + active_surface.width() / 2;
        let maximum_center = safe_right - active_surface.width() / 2;
        if maximum_center < minimum_center {
            return OverlayPlacement::Hidden;
        }
        preferred_center.clamp(minimum_center, maximum_center)
    };

    let client_offset_x = overlay.client_screen_left - overlay.window_rect.left;
    let client_offset_y = overlay.client_screen_top - overlay.window_rect.top;
    let desired_surface_left = surface_center - active_surface.width() / 2;
    let x = desired_surface_left - client_offset_x - active_surface.left;

    let desired_surface_top =
        caption.band.top + (caption.band.height() - expanded_surface.height()) / 2;
    let y = desired_surface_top - client_offset_y - expanded_surface.top;
    let placed_top = y + client_offset_y + expanded_surface.top;
    let placed_bottom = y + client_offset_y + expanded_surface.bottom;
    if placed_top < caption.band.top || placed_bottom > caption.band.bottom {
        return OverlayPlacement::Hidden;
    }

    OverlayPlacement::Visible { x, y, compact }
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
    fn regions_match_visible_surfaces() {
        assert_eq!(
            surface_rect(280, 40, OverlayMode::Collapsed),
            RectI {
                left: 94,
                top: 6,
                right: 186,
                bottom: 34,
            }
        );
        assert_eq!(
            surface_rect(280, 40, OverlayMode::Expanded),
            RectI {
                left: 4,
                top: 3,
                right: 276,
                bottom: 37,
            }
        );
        assert_eq!(
            surface_rect(280, 40, OverlayMode::Compact),
            RectI {
                left: 120,
                top: 3,
                right: 160,
                bottom: 37,
            }
        );
    }

    #[test]
    fn caption_bounds_are_converted_from_window_relative_space() {
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
        assert_eq!(geometry.band.top, 0);
        assert_eq!(geometry.band.bottom, 46);
        assert_eq!(geometry.buttons_left, 1677);
    }

    #[test]
    fn expanded_surface_is_fully_inside_caption_band() {
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
        );
        let OverlayPlacement::Visible { y, compact, .. } = placement else {
            panic!("expected visible placement");
        };
        assert!(!compact);
        let expanded = surface_rect(350, 50, OverlayMode::Expanded);
        assert!(y + expanded.top >= 0);
        assert!(y + expanded.bottom <= 46);
    }

    #[test]
    fn negative_monitor_coordinates_remain_valid() {
        let overlay = WindowMetrics {
            window_rect: RectI {
                left: 0,
                top: 0,
                right: 280,
                bottom: 40,
            },
            client_screen_left: 0,
            client_screen_top: 0,
            client_width: 280,
            client_height: 40,
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
        );
        assert!(matches!(placement, OverlayPlacement::Visible { .. }));
    }
}
