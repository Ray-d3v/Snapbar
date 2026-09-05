use std::{
    thread,
    time::{Duration, Instant},
};

use crate::{
    assets::Assets,
    capture::{
        CaptureEngine, CaptureSource, CaptureTarget, LocalMonitorCaptureTarget,
        save_clipboard_image_to_screenshots, show_capture_flash, suspend_capture_flash,
        windows_screenshots_folder,
    },
    meeting::{MeetingMonitor, MeetingSnapshot},
    overlay::{
        COLLAPSED_WIDTH, COMPACT_WIDTH, DEFAULT_TITLEBAR_COLOR, EXPANDED_HEIGHT, EXPANDED_WIDTH,
        HOVER_ISLAND_HEIGHT, INLINE_HEIGHT, INLINE_WIDTH, OverlayCaptureMode, OverlayPresentation,
        TITLEBAR_SURFACE_HEIGHT, TeamsWindowFollower, TitlebarMaterial, WINDOW_HEIGHT,
        WINDOW_WIDTH, disclosure_height, disclosure_width_for_attachment, island_drop_progress,
        paint_island_drop, presenter_disclosure_height,
    },
    resident::ResidentController,
    settings::AppSettings,
};
use gpui::{
    Animation, AnimationExt as _, App, Bounds, ClickEvent, Context, FontWeight, SpringAnimation,
    SpringConfig, Task, Window, WindowBackgroundAppearance, WindowBounds, WindowDecorations,
    WindowKind, WindowOptions, canvas, div, prelude::*, px, relative, rgb, rgba, size, svg,
    transparent_black,
};
use gpui_platform::application;

const RESIDENT_SYNC_INTERVAL: Duration = Duration::from_millis(250);
const SAVE_INFO_HOVER_DELAY: Duration = Duration::from_secs(1);
const CONTROL_HINT_SWITCH_DELAY: Duration = Duration::from_millis(160);
const COPY_FEEDBACK_DURATION: Duration = Duration::from_secs(2);
const SAVE_INFO_WIDTH: f32 = 204.0;
const RECORDABLE_OVERLAY_EXCLUSION_SETTLE: Duration = Duration::from_millis(34);
const DISCLOSURE_SPRING_STIFFNESS: f32 = 900.0;
const DISCLOSURE_SPRING_DAMPING: f32 = 46.0;
const COLLAPSE_SPRING_STIFFNESS: f32 = 450.0;
const COLLAPSE_SPRING_DAMPING: f32 = 40.0;
const INLINE_DISCLOSURE_SPRING_STIFFNESS: f32 = 360.0;
const INLINE_DISCLOSURE_SPRING_DAMPING: f32 = 28.0;
const INLINE_DISCLOSURE_BOUNCE_RESTITUTION: f32 = 0.55;
const DISCLOSURE_OVERSHOOT_GAIN: f32 = 2.0;
const DISCLOSURE_MAX_PRESENTATION: f32 = 1.044;
const TITLEBAR_SURFACE_REVEAL_END: f32 = 0.10;
const EXPANDED_CONTROL_GAP: f32 = 6.0;
const COLLAPSING_CONTROL_PITCH: f32 = 22.0;
const CONTEXT_PANEL_WIDTH: f32 = 82.0;
const ACTION_CONTROL_SIZE: f32 = 30.0;
const CONTROL_VISUAL_INSET: f32 = 1.0;
const CONTEXT_PANEL_VISUAL_WIDTH: f32 = CONTEXT_PANEL_WIDTH - CONTROL_VISUAL_INSET * 2.0;
const ACTION_VISUAL_SIZE: f32 = ACTION_CONTROL_SIZE - CONTROL_VISUAL_INSET * 2.0;
const STATUS_INDICATOR_SIZE: f32 = 6.0;
const CONTEXT_STATUS_SLOT: f32 = STATUS_INDICATOR_SIZE + 4.0;
const PERSISTENT_CAMERA_SIZE: f32 = 16.0;
const QUIT_PROMPT_WIDTH: f32 = 88.0;
const QUIT_CONFIRM_WIDTH: f32 = 56.0;
const QUIT_CANCEL_WIDTH: f32 = 68.0;
const QUIT_CONFIRM_GAP: f32 = 8.0;
const QUIT_CONFIRM_ROW_WIDTH: f32 =
    QUIT_PROMPT_WIDTH + QUIT_CONFIRM_WIDTH + QUIT_CANCEL_WIDTH + QUIT_CONFIRM_GAP * 2.0;
const CONTEXT_LABEL_TEXT_SIZE: f32 = 10.5;
const CONTEXT_LABEL_LINE_HEIGHT: f32 = 11.0;
const CONTEXT_DETAIL_TEXT_SIZE: f32 = 9.0;
const CONTEXT_DETAIL_LINE_HEIGHT: f32 = 9.0;
const CONTEXT_ROW_GAP: f32 = 1.0;
const CONTEXT_SHORTCUT_VERTICAL_PADDING: f32 = 1.0;
const CAPTURE_SHORTCUT_LABEL: &str = "Ctrl+Alt+S";
#[cfg(test)]
const MIN_EXPANDED_EDGE_GUTTER: f32 = 28.0;
const HOVER_CONTROLS_OFFSET_Y: f32 = 0.0;
const PRESENTER_IDLE_CONTENT_OFFSET_Y: f32 = 5.0;
const PRESENTER_CONTROLS_OFFSET_Y: f32 = 0.0;
const EXPANDED_CONTROLS_WIDTH: f32 =
    CONTEXT_PANEL_WIDTH + ACTION_CONTROL_SIZE * 4.0 + EXPANDED_CONTROL_GAP * 4.0;
const IDLE_CAPTURE_CENTER_X: f32 = -COLLAPSED_WIDTH / 2.0 + COMPACT_WIDTH + COMPACT_WIDTH / 2.0;
const EXPANDED_CAPTURE_CENTER_X_UNSHIFTED: f32 = -EXPANDED_CONTROLS_WIDTH / 2.0
    + CONTEXT_PANEL_WIDTH
    + EXPANDED_CONTROL_GAP
    + ACTION_CONTROL_SIZE
    + EXPANDED_CONTROL_GAP
    + ACTION_CONTROL_SIZE / 2.0;
const EXPANDED_CONTENT_SHIFT_X: f32 = IDLE_CAPTURE_CENTER_X - EXPANDED_CAPTURE_CENTER_X_UNSHIFTED;
const IDLE_STATUS_CENTER_X: f32 = -COLLAPSED_WIDTH / 2.0 + COMPACT_WIDTH / 2.0;
const EXPANDED_STATUS_CENTER_X: f32 = EXPANDED_CONTENT_SHIFT_X - EXPANDED_CONTROLS_WIDTH / 2.0
    + CONTROL_VISUAL_INSET
    + 4.0
    + STATUS_INDICATOR_SIZE / 2.0;

fn smoothstep_between(start: f32, end: f32, value: f32) -> f32 {
    let phase = ((value - start) / (end - start)).clamp(0.0, 1.0);
    phase * phase * (3.0 - 2.0 * phase)
}

fn disclosure_surface_alpha(presenter_attached: bool, content_progress: f32) -> f32 {
    if presenter_attached {
        return 1.0;
    }

    (1.0 / 255.0
        + (254.0 / 255.0) * smoothstep_between(0.0, TITLEBAR_SURFACE_REVEAL_END, content_progress))
    .clamp(1.0 / 255.0, 1.0)
}

fn disclosure_spring_config(
    presentation: OverlayPresentation,
    caption_collapsing: bool,
) -> SpringConfig {
    if presentation.is_inline() {
        SpringConfig::new(
            INLINE_DISCLOSURE_SPRING_STIFFNESS,
            INLINE_DISCLOSURE_SPRING_DAMPING,
            1.0,
        )
    } else if caption_collapsing {
        SpringConfig::new(COLLAPSE_SPRING_STIFFNESS, COLLAPSE_SPRING_DAMPING, 1.0)
    } else {
        SpringConfig::new(DISCLOSURE_SPRING_STIFFNESS, DISCLOSURE_SPRING_DAMPING, 1.0)
    }
}

fn inline_disclosure_progress(spring_position: f32) -> f32 {
    if spring_position <= 1.0 {
        spring_position.clamp(0.0, 1.0)
    } else {
        // Keep the title-bar silhouette bounded while retaining the tiny elastic
        // settle that makes the Dynamic Island motion feel alive.
        (1.0 - (spring_position - 1.0) * INLINE_DISCLOSURE_BOUNCE_RESTITUTION).max(0.0)
    }
}

fn disclosure_presentation_progress(
    spring_position: f32,
    presentation: OverlayPresentation,
) -> f32 {
    let spring_position = spring_position.max(0.0);
    if presentation.is_inline() {
        inline_disclosure_progress(spring_position)
    } else if spring_position > 1.0 {
        1.0 + (spring_position - 1.0) * DISCLOSURE_OVERSHOOT_GAIN
    } else {
        spring_position
    }
    .clamp(0.0, DISCLOSURE_MAX_PRESENTATION)
}

fn spring_position_for_progress(progress: f32, presentation: OverlayPresentation) -> f32 {
    let progress = progress.clamp(0.0, DISCLOSURE_MAX_PRESENTATION);
    if !presentation.is_inline() && progress > 1.0 {
        // The native region publishes the amplified visual bounce. Undo that
        // projection before using it as the next spring's initial position.
        1.0 + (progress - 1.0) / DISCLOSURE_OVERSHOOT_GAIN
    } else {
        progress.min(1.0)
    }
}

fn expanded_control_gap(content_progress: f32) -> f32 {
    EXPANDED_CONTROL_GAP * smoothstep_between(0.12, 0.94, content_progress)
}

#[derive(Clone, Copy)]
struct IslandContentMotion {
    center_y: f32,
    status_center_x: f32,
    status_center_y: f32,
    capture_form: f32,
    details_alpha: f32,
    auxiliary_alpha: f32,
    auxiliary_inset: f32,
}

fn island_content_motion(progress: f32) -> IslandContentMotion {
    let idle_center_y = TITLEBAR_SURFACE_HEIGHT / 2.0;
    let expanded_center_y = HOVER_ISLAND_HEIGHT / 2.0 + HOVER_CONTROLS_OFFSET_Y;
    let status_progress = smoothstep_between(0.0, 0.48, progress);
    IslandContentMotion {
        // The camera and its row ride the deforming lower edge. They never
        // cross-fade between two separate vertical positions.
        center_y: idle_center_y
            + (expanded_center_y - idle_center_y) * island_drop_progress(progress),
        // Use one interpolation for both coordinates, so the dot travels in a
        // straight line after the label fades instead of turning at the caption.
        status_center_x: IDLE_STATUS_CENTER_X
            + (EXPANDED_STATUS_CENTER_X - IDLE_STATUS_CENTER_X) * status_progress,
        status_center_y: idle_center_y + (expanded_center_y - idle_center_y) * status_progress,
        capture_form: island_drop_progress(progress),
        details_alpha: smoothstep_between(0.48, 0.92, progress),
        // Icons remain with the retracting surface after the explanation fades.
        // Converge while still inside the native region, then finish fading in
        // the caption band. This projection also stays continuous on reversal.
        auxiliary_alpha: smoothstep_between(0.15, 0.90, progress),
        auxiliary_inset: (ACTION_CONTROL_SIZE + EXPANDED_CONTROL_GAP - COLLAPSING_CONTROL_PITCH)
            * (1.0 - smoothstep_between(0.15, 1.0, progress)),
    }
}

fn mix_rgb(base: u32, target: u32, amount: f32) -> u32 {
    let amount = amount.clamp(0.0, 1.0);
    [16, 8, 0]
        .into_iter()
        .map(|shift| {
            let base_channel = ((base >> shift) & 0xff) as f32;
            let target_channel = ((target >> shift) & 0xff) as f32;
            ((base_channel + (target_channel - base_channel) * amount).round() as u32) << shift
        })
        .fold(0, |color, channel| color | channel)
}

fn relative_luminance(color: u32) -> f32 {
    let linear_channel = |shift: u32| {
        let value = ((color >> shift) & 0xff_u32) as f32 / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear_channel(16) + 0.7152 * linear_channel(8) + 0.0722 * linear_channel(0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TitlebarPalette {
    surface: u32,
    primary_text: u32,
    secondary_text: u32,
    idle_active_backplate: u32,
    control_hover: u32,
    quit_hover: u32,
    control_icon: u32,
    disabled_control: u32,
    disabled_icon: u32,
    save_hover: u32,
    save_icon: u32,
    danger_icon: u32,
    is_light: bool,
}

fn idle_camera_color(palette: TitlebarPalette, can_capture: bool) -> u32 {
    if can_capture {
        palette.primary_text
    } else {
        palette.secondary_text
    }
}

impl TitlebarPalette {
    fn for_surface(surface: u32) -> Self {
        let is_light = relative_luminance(surface) >= 0.5;
        if is_light {
            Self {
                surface,
                primary_text: 0x202124,
                secondary_text: 0x5f6368,
                idle_active_backplate: 0x0000001f,
                control_hover: mix_rgb(surface, 0x000000, 0.16),
                quit_hover: mix_rgb(surface, 0xc42b1c, 0.14),
                control_icon: 0x4f5058,
                disabled_control: mix_rgb(surface, 0x000000, 0.12),
                disabled_icon: 0x66666f,
                save_hover: mix_rgb(surface, 0x2e7d4a, 0.32),
                save_icon: 0x225c37,
                danger_icon: 0xb4232c,
                is_light,
            }
        } else {
            Self {
                surface,
                primary_text: 0xf5f5f6,
                secondary_text: 0x9b9ba2,
                idle_active_backplate: 0xffffff24,
                control_hover: mix_rgb(surface, 0xffffff, 0.16),
                quit_hover: mix_rgb(surface, 0xd1444c, 0.22),
                control_icon: 0xc8c8cd,
                disabled_control: mix_rgb(surface, 0xffffff, 0.14),
                disabled_icon: 0xa7a7ac,
                save_hover: mix_rgb(surface, 0x3a9b60, 0.55),
                save_icon: 0x69d894,
                danger_icon: 0xf07178,
                is_light,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureState {
    Idle,
    WaitingForShare,
    Capturing,
    Copied { until: Instant },
    NoTarget,
    Error,
}

impl CaptureState {
    fn after_feedback_timeout(self, now: Instant) -> Self {
        match self {
            Self::Copied { until } if now >= until => Self::Idle,
            state => state,
        }
    }

    fn with_error(self, has_error: bool) -> Self {
        if has_error { Self::Error } else { self }
    }

    fn icon_path(self) -> &'static str {
        match self {
            Self::Copied { .. } => "icons/check.svg",
            Self::Error => "icons/alert.svg",
            Self::Capturing => "icons/more.svg",
            _ => "icons/camera.svg",
        }
    }
}

fn capture_icon_color(
    state: CaptureState,
    palette: TitlebarPalette,
    can_capture: bool,
    expanded: f32,
) -> u32 {
    let idle_color = match state {
        CaptureState::Copied { .. } => {
            if palette.is_light {
                0x277a46
            } else {
                0x55c27c
            }
        }
        CaptureState::Error => palette.danger_icon,
        _ => return idle_camera_color(palette, can_capture),
    };
    mix_rgb(idle_color, 0xffffff, expanded)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExpandedControl {
    Capture,
    Save,
    Refresh,
    Quit,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ControlHover {
    current: Option<ExpandedControl>,
    hint: Option<ExpandedControl>,
}

impl ControlHover {
    fn display_hint(
        self,
        feedback: CaptureState,
        save_to_screenshots: bool,
    ) -> Option<ControlHint> {
        self.hint
            .filter(|_| feedback != CaptureState::Error)
            .map(|control| control.hover_hint(save_to_screenshots))
    }

    fn update(&mut self, control: ExpandedControl, hovered: bool) -> bool {
        let next = if hovered {
            Some(control)
        } else if self.current == Some(control) {
            None
        } else {
            // An old control's leave can arrive after the next control's enter.
            return false;
        };
        if self.current == next {
            return false;
        }
        self.current = next;
        // Gaps retain the explanation, but never count as hovering a button.
        // The first explanation is immediate; subsequent changes must settle.
        if self.hint.is_none() {
            self.hint = next;
        }
        true
    }

    fn pending_hint(self) -> Option<ExpandedControl> {
        self.current.filter(|current| Some(*current) != self.hint)
    }

    fn settle_hint(&mut self, control: ExpandedControl) -> bool {
        if self.pending_hint() != Some(control) {
            return false;
        }
        self.hint = Some(control);
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ControlHint {
    label: &'static str,
    detail: &'static str,
    is_shortcut: bool,
}

impl ExpandedControl {
    fn hover_hint(self, save_to_screenshots: bool) -> ControlHint {
        match self {
            Self::Capture => ControlHint {
                label: "撮影",
                detail: CAPTURE_SHORTCUT_LABEL,
                is_shortcut: true,
            },
            Self::Save => ControlHint {
                label: "PNG保存",
                detail: if save_to_screenshots { "ON" } else { "OFF" },
                is_shortcut: false,
            },
            Self::Refresh => ControlHint {
                label: "再検出",
                detail: "会議・共有",
                is_shortcut: false,
            },
            Self::Quit => ControlHint {
                label: "終了",
                detail: "Snapbar",
                is_shortcut: false,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct DisclosureAnimationState {
    generation: u64,
    start: f32,
}

impl DisclosureAnimationState {
    fn retarget(&mut self, current_progress: f32, presentation: OverlayPresentation) {
        // A new element id keeps the current visual position but intentionally
        // drops velocity inherited from the opposite hover direction.
        self.generation = self.generation.wrapping_add(1);
        self.start = spring_position_for_progress(current_progress, presentation);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum IslandPage {
    #[default]
    Controls,
    SaveInfo,
    ConfirmQuit,
}

#[derive(Clone, Copy)]
enum QuitAction {
    Request,
    Cancel,
    Confirm,
}

impl IslandPage {
    fn allows_save_info(
        self,
        hovered_control: Option<ExpandedControl>,
        expanded: bool,
        compact: bool,
        quitting: bool,
    ) -> bool {
        self == Self::Controls
            && hovered_control == Some(ExpandedControl::Save)
            && expanded
            && !compact
            && !quitting
    }

    /// Only an explicit confirmation on the confirmation page can exit.
    fn handle_quit(&mut self, action: QuitAction) -> bool {
        match action {
            QuitAction::Request => {
                *self = Self::ConfirmQuit;
                false
            }
            QuitAction::Cancel => {
                *self = Self::Controls;
                false
            }
            QuitAction::Confirm => std::mem::take(self) == Self::ConfirmQuit,
        }
    }
}

/// Serialize clipboard/save work and retain only the latest intent while busy.
/// This is independent of the displayed state, which can change on retargeting.
#[derive(Default)]
struct CaptureRequests {
    active: Option<u64>,
    pending: bool,
}

impl CaptureRequests {
    fn queue_if_active(&mut self) -> bool {
        if self.active.is_some() {
            self.pending = true;
            true
        } else {
            false
        }
    }

    fn start(&mut self, generation: u64) {
        debug_assert!(self.active.is_none());
        self.active = Some(generation);
    }

    fn finish(&mut self, generation: u64) -> bool {
        if self.active != Some(generation) {
            return false;
        }
        self.active = None;
        std::mem::take(&mut self.pending)
    }

    fn clear_pending(&mut self) {
        self.pending = false;
    }
}

struct Snapbar {
    presentation: OverlayPresentation,
    capture_mode: OverlayCaptureMode,
    targets: Vec<CaptureTarget>,
    selected_target: usize,
    capture_engine: Option<CaptureEngine>,
    follower: Option<TeamsWindowFollower>,
    meeting_monitor: MeetingMonitor,
    resident: ResidentController,
    last_monitor_generation: u64,
    shared_content_hint: bool,
    presenter_toolbar_id: Option<u32>,
    local_share_active: bool,
    local_monitor_target: Option<LocalMonitorCaptureTarget>,
    compact_layout: bool,
    titlebar_material: TitlebarMaterial,
    settings: AppSettings,
    expanded: bool,
    control_hover: ControlHover,
    control_hint_task: Option<Task<()>>,
    island_page: IslandPage,
    save_info_task: Option<Task<()>>,
    save_info_path: Option<Result<String, String>>,
    disclosure_animation: DisclosureAnimationState,
    capture_state: CaptureState,
    capture_generation: u64,
    capture_requests: CaptureRequests,
    last_error: Option<String>,
    quitting: bool,
}

impl Snapbar {
    fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        presentation: OverlayPresentation,
        capture_mode: OverlayCaptureMode,
    ) -> Self {
        window.set_window_title("Snapbar");
        let follower = TeamsWindowFollower::start(window, presentation, capture_mode);
        let overlay_events = follower.as_ref().map(TeamsWindowFollower::subscribe);
        let geometry_notifier = follower
            .as_ref()
            .map(TeamsWindowFollower::geometry_notifier);
        let meeting_monitor = MeetingMonitor::start(move |window_id| {
            geometry_notifier
                .as_ref()
                .is_some_and(|notifier| notifier.window_changed(window_id))
        });
        let meeting_events = meeting_monitor.subscribe();
        let titlebar_material = follower
            .as_ref()
            .map(TeamsWindowFollower::titlebar_material)
            .unwrap_or(TitlebarMaterial {
                surface: DEFAULT_TITLEBAR_COLOR,
                separator: DEFAULT_TITLEBAR_COLOR,
                separator_offset: 0,
            });
        let mut snapbar = Self {
            presentation,
            capture_mode,
            targets: Vec::new(),
            selected_target: 0,
            capture_engine: None,
            follower,
            meeting_monitor,
            resident: ResidentController::start(),
            last_monitor_generation: u64::MAX,
            shared_content_hint: false,
            presenter_toolbar_id: None,
            local_share_active: false,
            local_monitor_target: None,
            compact_layout: false,
            titlebar_material,
            settings: AppSettings::load(),
            expanded: false,
            control_hover: ControlHover::default(),
            control_hint_task: None,
            island_page: IslandPage::default(),
            save_info_task: None,
            save_info_path: None,
            disclosure_animation: DisclosureAnimationState::default(),
            capture_state: CaptureState::NoTarget,
            capture_generation: 0,
            capture_requests: CaptureRequests::default(),
            last_error: None,
            quitting: false,
        };
        let snapshot = snapbar.meeting_monitor.snapshot();
        snapbar.apply_meeting_snapshot(snapshot, cx);
        let capture_requests = snapbar.resident.capture_requests();
        snapbar.start_capture_hotkey_sync(capture_requests, window, cx);
        snapbar.start_meeting_sync(meeting_events, window, cx);
        snapbar.start_resident_sync(window, cx);
        if let Some(events) = overlay_events {
            snapbar.start_overlay_sync(events, window, cx);
        }
        snapbar
    }

    fn current_target(&self) -> Option<&CaptureTarget> {
        self.targets.get(self.selected_target)
    }

    fn current_capture_source(&self) -> Option<CaptureSource> {
        if let Some(target) = self.local_monitor_target.as_ref() {
            return Some(CaptureSource::LocalMonitor(target.clone()));
        }
        if self.shared_content_hint {
            self.current_target()
                .map(|target| CaptureSource::RemoteTeamsWindow(target.id))
        } else {
            None
        }
    }

    fn has_capture_context(&self) -> bool {
        self.current_target().is_some()
            || self.presenter_toolbar_id.is_some()
            || self.local_share_active
    }

    fn sync_follower(&self) {
        if let Some(follower) = &self.follower {
            follower.set_target(self.current_target().map(|target| target.id));
            follower.set_presenter_toolbar(self.presenter_toolbar_id);
        }
    }

    fn retarget_disclosure(&mut self, expanded: bool, current_progress: f32) -> bool {
        if !expanded {
            self.island_page.handle_quit(QuitAction::Cancel);
            self.save_info_task = None;
            self.save_info_path = None;
        }
        if self.expanded == expanded {
            return false;
        }

        self.expanded = expanded;
        if !expanded {
            self.reset_control_hover();
        }
        self.disclosure_animation
            .retarget(current_progress, self.presentation);
        true
    }

    fn reset_control_hover(&mut self) {
        self.control_hint_task = None;
        self.control_hover = ControlHover::default();
    }

    fn update_hovered_control(
        &mut self,
        control: ExpandedControl,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        if self.island_page != IslandPage::Controls {
            return;
        }
        if self.control_hover.update(control, hovered) {
            self.control_hint_task = None;
            self.save_info_task = None;
            if self.control_hover.current == Some(ExpandedControl::Save) {
                self.start_save_info_dwell(cx);
            }
            if let Some(control) = self.control_hover.pending_hint() {
                // This owned task changes text only. Native disclosure remains
                // the sole authority for the visible surface and input region.
                self.control_hint_task = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(CONTROL_HINT_SWITCH_DELAY)
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        let native_expanded = this.follower.as_ref().is_some_and(|follower| {
                            follower.is_visible()
                                && follower.is_expanded()
                                && !follower.is_compact()
                        });
                        if this.island_page == IslandPage::Controls
                            && native_expanded
                            && !this.quitting
                            && this.control_hover.settle_hint(control)
                        {
                            cx.notify();
                        }
                    });
                }));
            }
            cx.notify();
        }
    }

    fn start_save_info_dwell(&mut self, cx: &mut Context<Self>) {
        // Ownership cancels the delay/readback when hover or native disclosure
        // changes. This timer only changes page content, never the HWND region.
        self.save_info_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_INFO_HOVER_DELAY).await;
            let opened = this.update(cx, |this, cx| {
                let native_expanded = this.follower.as_ref().is_some_and(|follower| {
                    follower.is_visible() && follower.is_expanded() && !follower.is_compact()
                });
                if !this.island_page.allows_save_info(
                    this.control_hover.current,
                    this.expanded && native_expanded,
                    this.compact_layout,
                    this.quitting,
                ) {
                    return false;
                }
                this.island_page = IslandPage::SaveInfo;
                this.reset_control_hover();
                this.save_info_path = None;
                cx.notify();
                true
            });
            if !matches!(opened, Ok(true)) {
                return;
            }
            let path = cx
                .background_executor()
                .spawn(async {
                    windows_screenshots_folder()
                        .map(|path| path.to_string_lossy().into_owned())
                        .map_err(|error| error.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.island_page == IslandPage::SaveInfo {
                    this.save_info_path = Some(path);
                    cx.notify();
                }
            });
        }));
    }

    fn on_save_info_back_clicked(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_info_task = None;
        self.save_info_path = None;
        self.island_page = IslandPage::Controls;
        self.reset_control_hover();
        cx.notify();
    }

    fn start_resident_sync(&self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(RESIDENT_SYNC_INTERVAL).await;
                if this
                    .update(cx, |this, cx| this.sync_resident_state(cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn start_capture_hotkey_sync(
        &self,
        capture_requests: async_channel::Receiver<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            while capture_requests.recv().await.is_ok() {
                if this
                    .update_in(cx, |this, window, cx| {
                        this.start_capture(window, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn start_meeting_sync(
        &self,
        meeting_events: async_channel::Receiver<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            while meeting_events.recv().await.is_ok() {
                if this
                    .update(cx, |this, cx| this.sync_resident_state(cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn start_overlay_sync(
        &self,
        events: async_channel::Receiver<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            while events.recv().await.is_ok() {
                if this
                    .update(cx, |this, cx| {
                        if this.sync_overlay_state() {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn sync_resident_state(&mut self, cx: &mut Context<Self>) {
        if self.resident.quit_requested() {
            self.begin_quit(cx);
            return;
        }

        let mut changed = false;
        if self.resident.take_rescan_requested() {
            self.request_redetection();
            changed = true;
        }

        let snapshot = self.meeting_monitor.snapshot();
        if snapshot.generation != self.last_monitor_generation {
            self.apply_meeting_snapshot(snapshot, cx);
            changed = true;
        }

        changed |= self.sync_overlay_state();

        let previous_state = self.capture_state;
        // Reuse the existing resident tick; capture feedback needs no animation
        // loop or detached timer that could expire a later capture's result.
        self.capture_state = self.capture_state.after_feedback_timeout(Instant::now());
        match self.capture_engine.as_ref() {
            Some(engine) if engine.is_ready() => {
                if matches!(
                    self.capture_state,
                    CaptureState::NoTarget | CaptureState::WaitingForShare
                ) {
                    self.capture_state = CaptureState::Idle;
                    self.last_error = None;
                }
            }
            Some(_) if self.capture_state != CaptureState::Capturing => {
                self.capture_state = CaptureState::WaitingForShare;
            }
            Some(_) => {}
            None if !self.has_capture_context() => {
                self.capture_state = CaptureState::NoTarget;
            }
            None if self.capture_state != CaptureState::Capturing => {
                self.capture_state = CaptureState::WaitingForShare;
            }
            None => {}
        }
        changed |= previous_state != self.capture_state;

        if changed {
            cx.notify();
        }
    }

    fn apply_meeting_snapshot(&mut self, snapshot: MeetingSnapshot, cx: &mut Context<Self>) {
        self.last_monitor_generation = snapshot.generation;
        let previous_id = self.current_target().map(|target| target.id);
        let previous_source = self.current_capture_source();
        let next_id = snapshot.target.as_ref().map(|target| target.id);
        let previous_presenter_toolbar_id = self.presenter_toolbar_id;
        self.presenter_toolbar_id = snapshot.presenter_toolbar_id;
        self.shared_content_hint = snapshot.shared_content_hint;
        self.local_share_active = snapshot.local_share_active;
        self.local_monitor_target = snapshot.local_monitor_target;

        if previous_id != next_id || previous_presenter_toolbar_id != self.presenter_toolbar_id {
            self.retarget_disclosure(false, 0.0);
            self.compact_layout = false;
            cx.notify();
        }

        match snapshot.target {
            Some(target) => {
                self.targets = vec![target];
                self.selected_target = 0;
            }
            None => {
                self.targets.clear();
                self.selected_target = 0;
            }
        }
        self.sync_follower();
        self.resident
            .set_capture_hotkey_enabled(self.has_capture_context());

        let next_source = self.current_capture_source();
        if next_source.is_some()
            && (previous_source != next_source || self.capture_engine.is_none())
        {
            self.restart_capture_engine();
        } else if next_source.is_none() {
            self.capture_generation = self.capture_generation.wrapping_add(1);
            self.capture_requests.clear_pending();
            self.capture_engine = None;
            self.capture_state = if self.has_capture_context() {
                CaptureState::WaitingForShare
            } else {
                CaptureState::NoTarget
            };
            self.last_error = None;
        }

        if !self.has_capture_context() {
            self.retarget_disclosure(false, 0.0);
            self.compact_layout = false;
            cx.notify();
        }
    }

    fn restart_capture_engine(&mut self) {
        self.capture_generation = self.capture_generation.wrapping_add(1);
        self.capture_requests.clear_pending();
        self.capture_engine = None;
        self.sync_follower();
        let Some(source) = self.current_capture_source() else {
            self.capture_state = if self.has_capture_context() {
                CaptureState::WaitingForShare
            } else {
                CaptureState::NoTarget
            };
            self.last_error = None;
            return;
        };

        match CaptureEngine::start_source(source) {
            Ok(engine) => {
                self.capture_engine = Some(engine);
                self.capture_state = CaptureState::WaitingForShare;
                self.last_error = None;
            }
            Err(error) => {
                self.capture_state = CaptureState::Error;
                self.last_error = Some(error.to_string());
            }
        }
    }

    fn request_redetection(&mut self) {
        self.meeting_monitor.request_scan();
        if self.current_capture_source().is_some() {
            self.restart_capture_engine();
        }
    }

    fn sync_overlay_state(&mut self) -> bool {
        let Some(follower) = self.follower.as_ref() else {
            let changed = self.expanded || self.compact_layout;
            self.retarget_disclosure(false, 0.0);
            self.compact_layout = false;
            return changed;
        };

        let compact = follower.is_compact();
        let expanded = follower.is_visible() && follower.is_expanded() && !compact;
        let titlebar_material = follower.titlebar_material();
        let disclosure_progress = follower.disclosure_progress();
        let changed = self.compact_layout != compact
            || self.expanded != expanded
            || self.titlebar_material != titlebar_material;
        self.compact_layout = compact;
        self.retarget_disclosure(expanded, disclosure_progress);
        self.titlebar_material = titlebar_material;
        changed
    }

    fn on_refresh_clicked(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.request_redetection();
        cx.notify();
    }

    fn on_save_toggle_clicked(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let next = AppSettings {
            save_to_screenshots: !self.settings.save_to_screenshots,
        };
        match next.store() {
            Ok(()) => {
                self.settings = next;
                self.last_error = None;
            }
            Err(error) => {
                self.last_error = Some(format!("保存設定を更新できませんでした: {error}"));
            }
        }
        // Only the controls page restarts the dwell. On SaveInfo this task may
        // still be loading the path, and toggling must leave that read intact.
        if self.island_page == IslandPage::Controls {
            self.save_info_task = None;
            if self.control_hover.current == Some(ExpandedControl::Save) {
                self.start_save_info_dwell(cx);
            }
        }
        cx.notify();
    }

    fn begin_quit(&mut self, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        self.reset_control_hover();
        self.save_info_task = None;
        self.capture_generation = self.capture_generation.wrapping_add(1);
        self.capture_requests.clear_pending();
        if let Some(follower) = self.follower.take() {
            follower.begin_shutdown();
            drop(follower);
        }
        self.resident.set_capture_hotkey_enabled(false);
        self.capture_engine = None;
        cx.quit();
    }

    fn on_quit_clicked(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.expanded && !self.compact_layout && !self.quitting {
            self.save_info_task = None;
            self.save_info_path = None;
            self.island_page.handle_quit(QuitAction::Request);
            self.reset_control_hover();
            cx.notify();
        }
    }

    fn on_cancel_quit_clicked(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.island_page.handle_quit(QuitAction::Cancel);
        self.reset_control_hover();
        cx.notify();
    }

    fn on_confirm_quit_clicked(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.island_page.handle_quit(QuitAction::Confirm) {
            self.begin_quit(cx);
        }
    }

    fn on_capture_clicked(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.start_capture(window, cx);
    }

    fn start_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.quitting || self.capture_requests.queue_if_active() {
            return;
        }

        let Some(engine) = self.capture_engine.clone() else {
            self.meeting_monitor.request_scan();
            self.capture_state = if self.has_capture_context() {
                CaptureState::WaitingForShare
            } else {
                CaptureState::NoTarget
            };
            cx.notify();
            return;
        };
        if !engine.is_ready() {
            self.capture_state = CaptureState::WaitingForShare;
            self.last_error = Some("共有コンテンツを待機中です".to_string());
            cx.notify();
            return;
        }

        let local_monitor_capture = engine.is_local_monitor();
        let overlay_exclusion = if local_monitor_capture {
            match self
                .follower
                .as_ref()
                .map(TeamsWindowFollower::exclude_overlay_from_capture)
            {
                Some(Some(exclusion)) => Some(exclusion),
                Some(None) => {
                    self.capture_state = CaptureState::Error;
                    self.last_error = Some(
                        "Snapbarを共有画面から一時的に除外できなかったため撮影を停止しました"
                            .to_string(),
                    );
                    cx.notify();
                    return;
                }
                None => None,
            }
        } else {
            None
        };

        self.capture_generation = self.capture_generation.wrapping_add(1);
        let generation = self.capture_generation;
        self.capture_requests.start(generation);
        let save_to_screenshots = self.settings.save_to_screenshots;
        self.capture_state = CaptureState::Capturing;
        self.last_error = None;
        cx.notify();
        let wait_for_composition =
            local_monitor_capture && self.capture_mode == OverlayCaptureMode::Recordable;

        let task = cx.background_executor().spawn(async move {
            let mut replacement = None;
            let result = (|| {
                let overlay_exclusion = overlay_exclusion;
                // Keep both exclusions active through a possible session recovery.
                let flash_suspension = suspend_capture_flash()?;
                if wait_for_composition {
                    thread::sleep(RECORDABLE_OVERLAY_EXCLUSION_SETTLE);
                }
                let outcome = engine.copy_latest_to_clipboard();
                replacement = outcome.replacement;
                let receipt = outcome.result?;
                drop(overlay_exclusion);
                drop(flash_suspension);
                let save_result = save_to_screenshots.then(save_clipboard_image_to_screenshots);
                Ok::<_, anyhow::Error>((receipt, save_result))
            })();
            (replacement, result)
        });

        cx.spawn_in(window, async move |this, cx| {
            let (replacement, result) = task.await;
            this.update_in(cx, |this, window, cx| {
                let capture_again = this.capture_requests.finish(generation);
                if this.capture_generation != generation {
                    if capture_again && !this.quitting {
                        this.start_capture(window, cx);
                    }
                    return;
                }

                if let Some(engine) = replacement {
                    this.capture_engine = Some(engine);
                }
                match result {
                    Ok((receipt, save_result)) => {
                        if !capture_again {
                            show_capture_flash(
                                receipt.screen_rect,
                                receipt.target_window_id,
                                this.capture_mode.display_affinity(),
                            );
                        }
                        let _ = receipt.frame_age;
                        let _ = receipt.latency;
                        this.capture_state = CaptureState::Copied {
                            until: Instant::now() + COPY_FEEDBACK_DURATION,
                        };
                        this.last_error = None;

                        if let Some(Err(error)) = save_result {
                            this.last_error =
                                Some(format!("コピー済み / ファイル保存失敗: {error}"));
                        }
                    }
                    Err(error) => {
                        this.capture_state = CaptureState::Error;
                        this.last_error = Some(error.to_string());
                    }
                }
                if capture_again {
                    this.start_capture(window, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn status_label(&self) -> &'static str {
        if self.last_error.is_some() {
            return "要確認";
        }
        match self.capture_state {
            CaptureState::Idle => "準備完了",
            CaptureState::WaitingForShare => "共有待ち",
            CaptureState::Capturing => "撮影中",
            CaptureState::Copied { .. } => "準備完了",
            CaptureState::NoTarget => "会議待ち",
            CaptureState::Error => "要確認",
        }
    }

    fn status_color(&self, light_surface: bool) -> u32 {
        match (
            light_surface,
            self.capture_state.with_error(self.last_error.is_some()),
        ) {
            (true, CaptureState::Idle | CaptureState::Copied { .. }) => 0x277a46,
            (true, CaptureState::Capturing) => 0xc7363f,
            (true, CaptureState::WaitingForShare | CaptureState::NoTarget) => 0x986500,
            (true, CaptureState::Error) => 0xb4232c,
            (false, CaptureState::Idle | CaptureState::Copied { .. }) => 0x55c27c,
            (false, CaptureState::Capturing) => 0xe5484d,
            (false, CaptureState::WaitingForShare | CaptureState::NoTarget) => 0xe0a24a,
            (false, CaptureState::Error) => 0xf07178,
        }
    }
}

impl Render for Snapbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_capture = self
            .capture_engine
            .as_ref()
            .is_some_and(CaptureEngine::is_ready)
            && !self.quitting;
        let presentation = self.presentation;
        let presenter_attached = self.presenter_toolbar_id.is_some();
        let caption_morph = !presenter_attached && !presentation.is_inline();
        let confirming_quit = self.island_page == IslandPage::ConfirmQuit;
        let showing_save_info = self.island_page == IslandPage::SaveInfo;
        let showing_controls = self.island_page == IslandPage::Controls;
        let presentation_width = if presentation.is_inline() {
            INLINE_WIDTH
        } else {
            EXPANDED_WIDTH
        };
        let presentation_height = if presenter_attached {
            HOVER_ISLAND_HEIGHT
        } else if presentation.is_inline() {
            INLINE_HEIGHT
        } else {
            HOVER_ISLAND_HEIGHT
        };
        let caption_cell_height = TITLEBAR_SURFACE_HEIGHT;
        let idle_content_offset_y = if presenter_attached {
            PRESENTER_IDLE_CONTENT_OFFSET_Y
        } else {
            0.0
        };
        let save_to_screenshots = self.settings.save_to_screenshots;
        let feedback_state = self.capture_state.with_error(self.last_error.is_some());
        let capture_icon_path = feedback_state.icon_path();
        let control_hint = self
            .control_hover
            .display_hint(feedback_state, save_to_screenshots);
        let context_label = control_hint
            .map(|hint| hint.label)
            .unwrap_or_else(|| self.status_label());
        let material = self.titlebar_material;
        let palette = TitlebarPalette::for_surface(material.surface);
        let idle_palette = palette;
        let idle_status_color = self.status_color(idle_palette.is_light);
        let status_color = self.status_color(palette.is_light);
        let primary_text = rgb(palette.primary_text);
        let secondary_text = rgb(palette.secondary_text);
        let island_background = rgb(palette.surface);
        // A non-zero alpha keeps the layered HWND interactive while remaining visually
        // indistinguishable from the Teams title bar at rest.
        let idle_hit_surface = rgba((palette.surface << 8) | 0x01);
        let idle_active_backplate = rgba(idle_palette.idle_active_backplate);
        let capture_background = match feedback_state {
            CaptureState::NoTarget | CaptureState::WaitingForShare => rgb(palette.disabled_control),
            CaptureState::Capturing => rgb(0xc83f47),
            CaptureState::Error => rgb(0xd1444c),
            CaptureState::Idle => rgb(0xe5484d),
            CaptureState::Copied { .. } => rgb(0x277a46),
        };
        let idle_camera_icon = || {
            svg()
                .path(capture_icon_path)
                .size(px(16.0))
                .text_color(rgb(capture_icon_color(
                    feedback_state,
                    idle_palette,
                    can_capture,
                    0.0,
                )))
        };

        let compact_camera = div()
            .id("titlebar-surface")
            .relative()
            .w(px(COMPACT_WIDTH))
            .h(px(EXPANDED_HEIGHT))
            .bg(if presenter_attached {
                island_background
            } else {
                rgba(0x00000000)
            })
            .child(
                div()
                    .id("compact-capture-button")
                    .absolute()
                    .top(px(idle_content_offset_y))
                    .left(px(0.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(COMPACT_WIDTH))
                    .h(px(caption_cell_height))
                    .bg(idle_hit_surface)
                    .cursor_pointer()
                    .active(move |button| button.bg(idle_active_backplate))
                    .when(can_capture, |button| {
                        button.on_click(cx.listener(Self::on_capture_clicked))
                    })
                    .child(idle_camera_icon()),
            );

        let idle_content = div()
            .absolute()
            .top(px(idle_content_offset_y))
            .left(relative(0.5))
            .ml(px(-COLLAPSED_WIDTH / 2.0))
            .flex()
            .items_center()
            .w(px(COLLAPSED_WIDTH))
            .h(px(caption_cell_height))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(COMPACT_WIDTH))
                    .h_full()
                    .bg(idle_hit_surface)
                    .child(
                        div()
                            .size(px(STATUS_INDICATOR_SIZE))
                            .rounded_full()
                            .bg(rgb(idle_status_color)),
                    ),
            )
            .when(!caption_morph, |content| {
                content.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(COMPACT_WIDTH))
                        .h_full()
                        .bg(idle_hit_surface)
                        .cursor_pointer()
                        .child(idle_camera_icon()),
                )
            });

        let context_title = div()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(4.0))
            .text_size(px(CONTEXT_LABEL_TEXT_SIZE))
            .line_height(px(CONTEXT_LABEL_LINE_HEIGHT))
            .font_weight(FontWeight::MEDIUM)
            .text_color(primary_text)
            .when(control_hint.is_none() && !caption_morph, |title| {
                title.child(
                    div()
                        .size(px(STATUS_INDICATOR_SIZE))
                        .rounded_full()
                        .bg(rgb(status_color)),
                )
            })
            .child(context_label);

        let context_detail = control_hint.map(|hint| {
            div()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(CONTEXT_DETAIL_TEXT_SIZE))
                .line_height(px(CONTEXT_DETAIL_LINE_HEIGHT))
                .text_color(if hint.is_shortcut {
                    primary_text
                } else {
                    secondary_text
                })
                .when(hint.is_shortcut, |detail| {
                    detail
                        .px(px(3.0))
                        .py(px(CONTEXT_SHORTCUT_VERTICAL_PADDING))
                        .rounded(px(3.0))
                        .bg(rgb(palette.control_hover))
                })
                .child(hint.detail)
        });

        let context_panel = div()
            .id("context-panel")
            .flex()
            .items_center()
            .justify_center()
            .w(px(CONTEXT_PANEL_WIDTH))
            .h(px(ACTION_CONTROL_SIZE))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(CONTEXT_ROW_GAP))
                    .w(px(CONTEXT_PANEL_VISUAL_WIDTH))
                    .h(px(ACTION_VISUAL_SIZE))
                    .px(px(4.0))
                    .when(caption_morph, |panel| {
                        panel.pl(px(4.0 + CONTEXT_STATUS_SLOT))
                    })
                    .overflow_hidden()
                    .child(context_title)
                    .when_some(context_detail, |panel, detail| panel.child(detail)),
            );

        let capture = div()
            .id("capture-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(ACTION_CONTROL_SIZE))
            .on_hover(cx.listener(|this, hovered, _window, cx| {
                this.update_hovered_control(ExpandedControl::Capture, *hovered, cx);
            }))
            .when(can_capture, |button| {
                button
                    .cursor_pointer()
                    .when(!caption_morph, |button| {
                        button.hover(|button| button.opacity(0.91))
                    })
                    .active(|button| button.opacity(0.68))
                    .on_click(cx.listener(Self::on_capture_clicked))
            })
            .when(!can_capture && !caption_morph, |button| {
                button.opacity(0.66)
            });
        let (row_capture, morph_capture) = if caption_morph {
            // Reserve the final row slot; the same camera remains above the
            // surface throughout disclosure, including the transparent idle state.
            (
                div().size(px(ACTION_CONTROL_SIZE)).into_any_element(),
                Some(capture),
            )
        } else {
            let capture = capture.child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(ACTION_VISUAL_SIZE))
                    .rounded_full()
                    .bg(capture_background)
                    .shadow_sm()
                    .child(svg().path(capture_icon_path).size(px(17.0)).text_color(
                        if can_capture
                            || matches!(
                                feedback_state,
                                CaptureState::Capturing | CaptureState::Error
                            )
                        {
                            rgb(0xffffff)
                        } else {
                            rgb(palette.disabled_icon)
                        },
                    )),
            );
            (capture.into_any_element(), None)
        };

        let save = div()
            .id("save-toggle")
            .flex()
            .items_center()
            .justify_center()
            .size(px(ACTION_CONTROL_SIZE))
            .cursor_pointer()
            .active(|button| button.opacity(0.72))
            .on_hover(cx.listener(|this, hovered, _window, cx| {
                this.update_hovered_control(ExpandedControl::Save, *hovered, cx);
            }))
            .on_click(cx.listener(Self::on_save_toggle_clicked))
            .child(
                div()
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(ACTION_VISUAL_SIZE))
                    .rounded_full()
                    .bg(
                        if self.control_hover.current == Some(ExpandedControl::Save) {
                            if save_to_screenshots {
                                rgb(palette.save_hover)
                            } else {
                                rgb(palette.control_hover)
                            }
                        } else {
                            rgba(0x00000000)
                        },
                    )
                    .child(svg().path("icons/folder.svg").size(px(16.0)).text_color(
                        if save_to_screenshots {
                            rgb(palette.save_icon)
                        } else {
                            rgb(palette.control_icon)
                        },
                    ))
                    .when(save_to_screenshots, |button| {
                        button.child(
                            div()
                                .absolute()
                                .bottom(px(2.0))
                                .left(px(ACTION_VISUAL_SIZE / 2.0 - 1.5))
                                .size(px(3.0))
                                .rounded_full()
                                .bg(rgb(palette.save_icon)),
                        )
                    }),
            );

        let refresh = div()
            .id("refresh-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(ACTION_CONTROL_SIZE))
            .cursor_pointer()
            .active(|button| button.opacity(0.72))
            .on_hover(cx.listener(|this, hovered, _window, cx| {
                this.update_hovered_control(ExpandedControl::Refresh, *hovered, cx);
            }))
            .on_click(cx.listener(Self::on_refresh_clicked))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(ACTION_VISUAL_SIZE))
                    .rounded_full()
                    .bg(
                        if self.control_hover.current == Some(ExpandedControl::Refresh) {
                            rgb(palette.control_hover)
                        } else {
                            rgba(0x00000000)
                        },
                    )
                    .child(
                        svg()
                            .path("icons/refresh.svg")
                            .size(px(16.0))
                            .text_color(rgb(palette.control_icon)),
                    ),
            );

        let quit = div()
            .id("quit-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(ACTION_CONTROL_SIZE))
            .cursor_pointer()
            .active(|button| button.opacity(0.72))
            .on_hover(cx.listener(|this, hovered, _window, cx| {
                this.update_hovered_control(ExpandedControl::Quit, *hovered, cx);
            }))
            .on_click(cx.listener(Self::on_quit_clicked))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(ACTION_VISUAL_SIZE))
                    .rounded_full()
                    .bg(
                        if self.control_hover.current == Some(ExpandedControl::Quit) {
                            rgb(palette.quit_hover)
                        } else {
                            rgba(0x00000000)
                        },
                    )
                    .child(
                        svg()
                            .path("icons/power.svg")
                            .size(px(16.0))
                            .text_color(rgb(palette.danger_icon)),
                    ),
            );

        let expanded_content = div()
            .absolute()
            .top(px(if presenter_attached {
                PRESENTER_CONTROLS_OFFSET_Y
            } else if presentation.is_inline() {
                0.0
            } else {
                HOVER_CONTROLS_OFFSET_Y
            }))
            .left(relative(0.5))
            .ml(px(-EXPANDED_WIDTH / 2.0 + EXPANDED_CONTENT_SHIFT_X))
            .flex()
            .items_center()
            .justify_center()
            .w(px(presentation_width))
            .h(px(presentation_height))
            .px(px(7.0));

        let quit_confirmation = div()
            .id("quit-confirmation")
            .absolute()
            .left(relative(0.5))
            .ml(px(-EXPANDED_WIDTH / 2.0))
            .top(px(if presenter_attached {
                PRESENTER_CONTROLS_OFFSET_Y
            } else if presentation.is_inline() {
                0.0
            } else {
                HOVER_CONTROLS_OFFSET_Y
            }))
            .flex()
            .items_center()
            .justify_center()
            .w(px(EXPANDED_WIDTH))
            .h(px(presentation_height))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(QUIT_CONFIRM_GAP))
                    .w(px(QUIT_CONFIRM_ROW_WIDTH))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .w(px(QUIT_PROMPT_WIDTH))
                            .h(px(ACTION_VISUAL_SIZE))
                            .text_size(px(10.5))
                            .line_height(px(12.0))
                            .text_color(primary_text)
                            .child("Snapbarを")
                            .child("終了しますか？"),
                    )
                    .child(
                        div()
                            .id("confirm-quit-button")
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(QUIT_CONFIRM_WIDTH))
                            .h(px(ACTION_VISUAL_SIZE))
                            .rounded(px(8.0))
                            .bg(rgb(0xc83f47))
                            .text_color(rgb(0xffffff))
                            .text_size(px(11.0))
                            .font_weight(FontWeight::MEDIUM)
                            .cursor_pointer()
                            .hover(|button| button.bg(rgb(0xd84a52)))
                            .active(|button| button.opacity(0.72))
                            .on_click(cx.listener(Self::on_confirm_quit_clicked))
                            .child("終了"),
                    )
                    // The old power icon is under Cancel, so a second click at
                    // the same point cannot accidentally confirm an exit.
                    .child(
                        div()
                            .id("cancel-quit-button")
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(QUIT_CANCEL_WIDTH))
                            .h(px(ACTION_VISUAL_SIZE))
                            .rounded(px(8.0))
                            .bg(rgb(palette.control_hover))
                            .text_color(primary_text)
                            .text_size(px(10.5))
                            .cursor_pointer()
                            .hover(|button| {
                                button.bg(rgb(mix_rgb(palette.surface, palette.primary_text, 0.24)))
                            })
                            .active(|button| button.opacity(0.72))
                            .on_click(cx.listener(Self::on_cancel_quit_clicked))
                            .child("キャンセル"),
                    ),
            );
        let save_info = showing_save_info.then(|| {
            let save_path = match &self.save_info_path {
                Some(Ok(path)) => path.clone(),
                Some(Err(_)) => "保存先を取得できませんでした".to_string(),
                None => "保存先を取得中…".to_string(),
            };
            let inline = presentation.is_inline() && !presenter_attached;
            let toggle_height = if inline { 14.0 } else { 20.0 };
            let path_height = if inline { 12.0 } else { 14.0 };
            let row_gap = if inline { 0.0 } else { 2.0 };
            let info_height = toggle_height + path_height + row_gap;
            div()
                .id("save-info")
                .absolute()
                .left(relative(0.5))
                .ml(px(-SAVE_INFO_WIDTH / 2.0))
                .top(px((presentation_height - info_height) / 2.0))
                .flex()
                .flex_col()
                .gap(px(row_gap))
                .w(px(SAVE_INFO_WIDTH))
                .h(px(info_height))
                .text_color(primary_text)
                .child(
                    div()
                        .relative()
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_shrink_0()
                        .h(px(toggle_height))
                        .whitespace_nowrap()
                        .line_height(px(if inline { 12.0 } else { 14.0 }))
                        .child(
                            div()
                                .id("save-info-toggle")
                                .flex()
                                .items_center()
                                .justify_center()
                                .gap(px(8.0))
                                .w(px(132.0))
                                .h(px(toggle_height))
                                .cursor_pointer()
                                .hover(|control| control.opacity(0.8))
                                .active(|control| control.opacity(0.65))
                                .on_click(cx.listener(Self::on_save_toggle_clicked))
                                .child(
                                    div()
                                        .text_size(px(if inline { 10.5 } else { 12.0 }))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child("PNG保存"),
                                )
                                .child(
                                    div()
                                        .relative()
                                        .flex_shrink_0()
                                        .w(px(30.0))
                                        .h(px(if inline { 12.0 } else { 16.0 }))
                                        .rounded_full()
                                        .bg(rgb(if save_to_screenshots {
                                            palette.save_icon
                                        } else {
                                            palette.control_hover
                                        }))
                                        .child(
                                            div()
                                                .absolute()
                                                .top(px(2.0))
                                                .left(px(if save_to_screenshots {
                                                    if inline { 20.0 } else { 16.0 }
                                                } else {
                                                    2.0
                                                }))
                                                .size(px(if inline { 8.0 } else { 12.0 }))
                                                .rounded_full()
                                                .bg(rgb(if save_to_screenshots {
                                                    0xffffff
                                                } else {
                                                    palette.primary_text
                                                })),
                                        ),
                                )
                                .child(
                                    div()
                                        .w(px(22.0))
                                        .text_size(px(if inline { 10.0 } else { 11.0 }))
                                        .text_color(secondary_text)
                                        .child(if save_to_screenshots { "ON" } else { "OFF" }),
                                ),
                        )
                        .child(
                            div()
                                .id("save-info-back")
                                .absolute()
                                .left(px(0.0))
                                .top(px(0.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .w(px(24.0))
                                .h(px(toggle_height))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .hover(|button| button.bg(rgb(palette.control_hover)))
                                .on_click(cx.listener(Self::on_save_info_back_clicked))
                                .child(
                                    svg()
                                        .path("icons/chevron-left.svg")
                                        .size(px(12.0))
                                        .text_color(secondary_text),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_shrink_0()
                        .h(px(path_height))
                        .child(
                            div()
                                .id("save-info-details")
                                .max_w(px(SAVE_INFO_WIDTH))
                                .min_w(px(0.0))
                                .h(px(path_height))
                                .overflow_x_scroll()
                                .text_color(secondary_text)
                                .text_size(px(if inline { 10.5 } else { 11.0 }))
                                .line_height(px(path_height))
                                .whitespace_nowrap()
                                .child(save_path),
                        ),
                )
                .with_animation(
                    "save-info-enter",
                    Animation::new(Duration::from_millis(140)),
                    |content, progress| content.opacity(progress),
                )
        });
        let morph_capture = morph_capture.filter(|_| showing_controls);

        let disclosure_target = if self.expanded { 1.0 } else { 0.0 };
        let disclosure_start = self.disclosure_animation.start;
        let disclosure_generation = self.disclosure_animation.generation;
        let caption_collapsing = caption_morph && !self.expanded;
        let progress_publisher = self
            .follower
            .as_ref()
            .map(TeamsWindowFollower::disclosure_progress_publisher);
        let animated_surface = div()
            .id("titlebar-surface")
            .relative()
            .w(px(EXPANDED_WIDTH))
            .h(px(EXPANDED_HEIGHT))
            .bg(transparent_black())
            .with_spring(
                ("titlebar-disclosure-spring", disclosure_generation),
                SpringAnimation::new(disclosure_spring_config(presentation, caption_collapsing))
                    .to(disclosure_target)
                    .from(disclosure_start)
                    .with_epsilon(0.001),
                move |surface, spring_position| {
                    let spring_position = spring_position.max(0.0);
                    let progress = disclosure_presentation_progress(spring_position, presentation);
                    let content_progress = spring_position.clamp(0.0, 1.0);
                    if let Some(publisher) = &progress_publisher {
                        publisher.publish(progress);
                    }

                    let surface_width =
                        disclosure_width_for_attachment(progress, presentation, presenter_attached);
                    let surface_height = if presenter_attached {
                        presenter_disclosure_height(progress)
                    } else if presentation.is_inline() {
                        INLINE_HEIGHT
                    } else {
                        disclosure_height(progress)
                    };
                    let surface_alpha =
                        disclosure_surface_alpha(presenter_attached, content_progress);
                    let morph = island_content_motion(content_progress);
                    let idle_alpha = 1.0 - smoothstep_between(0.12, 0.62, content_progress);
                    let expanded_alpha = if caption_morph {
                        1.0
                    } else {
                        smoothstep_between(0.16, 0.86, content_progress)
                    };
                    let control_gap = if caption_morph {
                        EXPANDED_CONTROL_GAP
                    } else {
                        expanded_control_gap(content_progress)
                    };
                    let context_alpha = if caption_morph {
                        morph.details_alpha
                    } else {
                        1.0
                    };
                    let auxiliary_alpha = if caption_morph {
                        morph.auxiliary_alpha
                    } else {
                        1.0
                    };
                    let auxiliary_inset = if caption_morph {
                        morph.auxiliary_inset
                    } else {
                        0.0
                    };

                    surface
                        .child(
                            div()
                                .absolute()
                                .top(px(0.0))
                                .left(relative(0.5))
                                .ml(px(-surface_width / 2.0))
                                .w(px(surface_width))
                                .h(px(surface_height))
                                // Keep the actual Teams caption visible. An opaque
                                // rectangle over it creates a seam even with a small
                                // color-sampling difference. Only the growing drop
                                // extends the sampled caption material below its edge.
                                .bg(if caption_morph {
                                    idle_hit_surface
                                } else {
                                    island_background.opacity(surface_alpha)
                                }),
                        )
                        .when(caption_morph, |surface| {
                            surface.child(
                                canvas(
                                    |_, _, _| (),
                                    move |bounds, _, window, _| {
                                        paint_island_drop(bounds, progress, material, window);
                                    },
                                )
                                .absolute()
                                // Align this canvas to the native client, including
                                // its transparent frame inset and side gutters.
                                .left(px((EXPANDED_WIDTH - WINDOW_WIDTH) / 2.0))
                                .top(px((EXPANDED_HEIGHT - WINDOW_HEIGHT) / 2.0))
                                .w(px(WINDOW_WIDTH))
                                .h(px(WINDOW_HEIGHT)),
                            )
                        })
                        .when(confirming_quit, |surface| {
                            surface.child(quit_confirmation.when(caption_morph, |content| {
                                content.top(px(morph.center_y - presentation_height / 2.0))
                            }))
                        })
                        .when_some(save_info, |surface, content| surface.child(content))
                        .when(
                            showing_controls && !caption_morph && content_progress < 0.72,
                            |surface| surface.child(idle_content.opacity(idle_alpha)),
                        )
                        .when(
                            showing_controls && expanded_alpha > 0.0 && auxiliary_alpha > 0.0,
                            |surface| {
                                surface.child(
                                    expanded_content
                                        .when(caption_morph, |content| {
                                            content
                                                .top(px(morph.center_y - presentation_height / 2.0))
                                        })
                                        .gap(px(control_gap))
                                        .opacity(expanded_alpha)
                                        .child(context_panel.opacity(context_alpha))
                                        .child(
                                            save.relative()
                                                .left(px(auxiliary_inset))
                                                .opacity(auxiliary_alpha),
                                        )
                                        .child(row_capture)
                                        .child(
                                            refresh
                                                .relative()
                                                .left(px(-auxiliary_inset))
                                                .opacity(auxiliary_alpha),
                                        )
                                        .child(
                                            quit.relative()
                                                .left(px(-2.0 * auxiliary_inset))
                                                .opacity(auxiliary_alpha),
                                        ),
                                )
                            },
                        )
                        .when(showing_controls && caption_morph, |surface| {
                            surface.child(
                                div()
                                    .id("status-indicator")
                                    .absolute()
                                    .left(relative(0.5))
                                    .ml(px(morph.status_center_x - STATUS_INDICATOR_SIZE / 2.0))
                                    .top(px(morph.status_center_y - STATUS_INDICATOR_SIZE / 2.0))
                                    .size(px(STATUS_INDICATOR_SIZE))
                                    .rounded_full()
                                    .bg(rgb(status_color)),
                            )
                        })
                        .when_some(morph_capture, |surface, capture| {
                            let hit_width = COMPACT_WIDTH
                                + (ACTION_CONTROL_SIZE - COMPACT_WIDTH) * morph.capture_form;
                            let hit_height = TITLEBAR_SURFACE_HEIGHT
                                + (ACTION_CONTROL_SIZE - TITLEBAR_SURFACE_HEIGHT)
                                    * morph.capture_form;
                            let icon_color = capture_icon_color(
                                feedback_state,
                                palette,
                                can_capture,
                                morph.capture_form,
                            );
                            // The plate contracts into the persistent glyph. Its
                            // opacity never hands off to a second camera element.
                            let plate_size = ACTION_VISUAL_SIZE * morph.capture_form.sqrt();
                            surface.child(
                                capture
                                    .absolute()
                                    .left(relative(0.5))
                                    .ml(px(IDLE_CAPTURE_CENTER_X - hit_width / 2.0))
                                    .top(px(morph.center_y - hit_height / 2.0))
                                    .w(px(hit_width))
                                    .h(px(hit_height))
                                    .bg(idle_hit_surface)
                                    .child(
                                        div()
                                            .relative()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .size(px(ACTION_VISUAL_SIZE))
                                            .child(
                                                div()
                                                    .absolute()
                                                    .left(relative(0.5))
                                                    .top(relative(0.5))
                                                    .ml(px(-plate_size / 2.0))
                                                    .mt(px(-plate_size / 2.0))
                                                    .size(px(plate_size))
                                                    .rounded_full()
                                                    .bg(capture_background.opacity(
                                                        if can_capture { 1.0 } else { 0.66 },
                                                    )),
                                            )
                                            .child(
                                                svg()
                                                    .path(capture_icon_path)
                                                    .size(px(PERSISTENT_CAMERA_SIZE))
                                                    .text_color(rgb(icon_color)),
                                            ),
                                    ),
                            )
                        })
                },
            );

        let surface = if self.compact_layout {
            compact_camera.into_any_element()
        } else {
            animated_surface.into_any_element()
        };

        div()
            .id("titlebar-overlay-root")
            .flex()
            .items_center()
            .justify_center()
            .size_full()
            // SetWindowRgn clips both painting and input to the island silhouette. Everything
            // outside it remains fully transparent so Teams keeps its title-bar drag surface.
            .bg(transparent_black())
            .child(surface)
    }
}

pub fn run() {
    let presentation = OverlayPresentation::from_command_line();
    let capture_mode = OverlayCaptureMode::from_command_line();
    application().with_assets(Assets).run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                focus: false,
                // This popup intentionally never activates. GPUI otherwise throttles
                // inactive windows to 33.3 ms frames, which makes hover disclosure
                // render at roughly 30 fps even on a high-refresh-rate display.
                inactive_frame_interval: None,
                kind: WindowKind::PopUp,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                window_background: WindowBackgroundAppearance::Transparent,
                window_decorations: Some(WindowDecorations::Client),
                ..Default::default()
            },
            move |window, cx| cx.new(|cx| Snapbar::new(window, cx, presentation, capture_mode)),
        )
        .expect("Snapbar window could not be created");

        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_capture_coalesces_click_and_hotkey_bursts_into_one_followup() {
        let mut requests = CaptureRequests::default();
        assert!(!requests.queue_if_active());
        requests.start(1);
        for _ in 0..40 {
            assert!(requests.queue_if_active());
        }
        assert!(requests.finish(1));
        requests.start(2);
        assert!(!requests.finish(2));
        assert!(requests.active.is_none());
        assert!(!requests.pending);
    }

    #[test]
    fn retargeting_discards_old_intent_but_keeps_running_work_serialized() {
        let mut requests = CaptureRequests::default();
        requests.start(1);
        assert!(requests.queue_if_active());
        requests.clear_pending();
        assert_eq!(requests.active, Some(1));
        assert!(!requests.finish(1));

        requests.start(2);
        requests.clear_pending();
        // A deliberate click after retargeting is retained until old work ends.
        assert!(requests.queue_if_active());
        assert!(!requests.finish(1));
        assert_eq!(requests.active, Some(2));
        assert!(requests.finish(2));
    }

    #[test]
    fn copy_feedback_stays_visible_while_a_safety_zone_keeps_the_hint() {
        let now = Instant::now();
        let mut hover = ControlHover::default();
        hover.update(ExpandedControl::Save, true);
        hover.update(ExpandedControl::Save, false);
        let copied = CaptureState::Copied {
            until: now + COPY_FEEDBACK_DURATION,
        };

        assert_eq!(copied.icon_path(), "icons/check.svg");
        assert_eq!(
            hover.display_hint(copied, true),
            Some(ExpandedControl::Save.hover_hint(true))
        );
        let ready = copied.after_feedback_timeout(now + COPY_FEEDBACK_DURATION);
        assert_eq!(ready.icon_path(), "icons/camera.svg");
        assert_eq!(
            hover.display_hint(ready, true),
            hover.display_hint(copied, true)
        );
    }

    #[test]
    fn another_capture_gets_its_full_feedback_duration() {
        let now = Instant::now();
        let first = CaptureState::Copied {
            until: now + COPY_FEEDBACK_DURATION,
        };
        let second = CaptureState::Copied {
            until: now + COPY_FEEDBACK_DURATION * 2,
        };

        assert_eq!(
            first.after_feedback_timeout(now + COPY_FEEDBACK_DURATION),
            CaptureState::Idle
        );
        assert_eq!(
            second.after_feedback_timeout(now + COPY_FEEDBACK_DURATION),
            second
        );
        assert_eq!(
            second.after_feedback_timeout(now + COPY_FEEDBACK_DURATION * 2),
            CaptureState::Idle
        );
        for next in [
            CaptureState::Capturing,
            CaptureState::Error,
            CaptureState::WaitingForShare,
            CaptureState::NoTarget,
        ] {
            assert_eq!(
                next.after_feedback_timeout(now + COPY_FEEDBACK_DURATION * 3),
                next
            );
        }
    }

    #[test]
    fn save_errors_override_success_and_hints_without_losing_the_safety_zone() {
        let copied = CaptureState::Copied {
            until: Instant::now() + COPY_FEEDBACK_DURATION,
        };
        let mut hover = ControlHover::default();
        hover.update(ExpandedControl::Save, true);
        hover.update(ExpandedControl::Save, false);
        let warning = copied.with_error(true);

        assert_eq!(warning.icon_path(), "icons/alert.svg");
        assert_eq!(hover.display_hint(warning, true), None);
        assert_eq!(
            hover.display_hint(copied.with_error(false), true),
            Some(ExpandedControl::Save.hover_hint(true))
        );
    }

    #[test]
    fn feedback_icons_have_contrast_in_both_caption_themes() {
        let copied = CaptureState::Copied {
            until: Instant::now() + COPY_FEEDBACK_DURATION,
        };
        for surface in [0x111111, 0xf3f3f3] {
            let palette = TitlebarPalette::for_surface(surface);
            for state in [copied, CaptureState::Error] {
                assert!(
                    contrast_ratio(capture_icon_color(state, palette, true, 0.0), surface) >= 3.0
                );
            }
            assert!(
                contrast_ratio(capture_icon_color(copied, palette, true, 1.0), 0x277a46) >= 4.5
            );
        }
    }

    #[test]
    fn gaps_keep_the_hint_without_extending_the_save_hover() {
        let mut hover = ControlHover::default();
        hover.update(ExpandedControl::Save, true);
        hover.update(ExpandedControl::Save, false);
        assert_eq!(hover.hint, Some(ExpandedControl::Save));
        assert_eq!(hover.current, None);
        assert_eq!(hover.pending_hint(), None);
        assert!(!IslandPage::Controls.allows_save_info(hover.current, true, false, false));

        hover.update(ExpandedControl::Capture, true);
        assert_eq!(hover.hint, Some(ExpandedControl::Save));
        assert!(!IslandPage::Controls.allows_save_info(hover.current, true, false, false));
        assert!(hover.settle_hint(ExpandedControl::Capture));
        assert_eq!(hover.hint, Some(ExpandedControl::Capture));
    }

    #[test]
    fn crossing_buttons_does_not_publish_transient_explanations() {
        let mut hover = ControlHover::default();
        hover.update(ExpandedControl::Capture, true);
        hover.update(ExpandedControl::Capture, false);
        for control in [ExpandedControl::Save, ExpandedControl::Refresh] {
            hover.update(control, true);
            hover.update(control, false);
            assert!(!hover.settle_hint(control));
            assert_eq!(hover.hint, Some(ExpandedControl::Capture));
        }
        hover.update(ExpandedControl::Quit, true);
        assert!(!hover.settle_hint(ExpandedControl::Refresh));
        assert_eq!(hover.hint, Some(ExpandedControl::Capture));
        assert!(hover.settle_hint(ExpandedControl::Quit));
        assert_eq!(hover.hint, Some(ExpandedControl::Quit));
    }

    #[test]
    fn late_leave_and_duplicate_enter_do_not_cancel_the_new_hint() {
        let mut hover = ControlHover::default();
        hover.update(ExpandedControl::Save, true);
        hover.update(ExpandedControl::Refresh, true);
        assert!(!hover.update(ExpandedControl::Save, false));
        assert!(!hover.update(ExpandedControl::Refresh, true));
        assert_eq!(hover.current, Some(ExpandedControl::Refresh));
        assert!(hover.settle_hint(ExpandedControl::Refresh));

        hover.update(ExpandedControl::Quit, true);
        hover = ControlHover::default();
        assert!(!hover.settle_hint(ExpandedControl::Quit));
        assert_eq!(hover.hint, None);
    }

    #[test]
    fn delayed_save_help_requires_the_same_visible_control_at_the_deadline() {
        let controls = IslandPage::Controls;
        assert!(controls.allows_save_info(Some(ExpandedControl::Save), true, false, false));
        // Leaving Save, moving to another action, collapsing/hiding, switching
        // to compact, or quitting must all invalidate a pending dwell.
        for hovered in [
            None,
            Some(ExpandedControl::Capture),
            Some(ExpandedControl::Quit),
        ] {
            assert!(!controls.allows_save_info(hovered, true, false, false));
        }
        for (expanded, compact, quitting) in [
            (false, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            assert!(!controls.allows_save_info(
                Some(ExpandedControl::Save),
                expanded,
                compact,
                quitting,
            ));
        }
        for page in [IslandPage::SaveInfo, IslandPage::ConfirmQuit] {
            assert!(!page.allows_save_info(Some(ExpandedControl::Save), true, false, false));
        }
    }

    #[test]
    fn save_help_cannot_confirm_an_exit_and_dismisses_to_controls() {
        let mut page = IslandPage::SaveInfo;
        assert!(!page.handle_quit(QuitAction::Confirm));
        assert_eq!(page, IslandPage::Controls);
        page = IslandPage::SaveInfo;
        assert!(!page.handle_quit(QuitAction::Cancel));
        assert_eq!(page, IslandPage::Controls);
    }

    #[test]
    fn requesting_quit_never_exits_until_the_confirmation_action() {
        let mut page = IslandPage::default();
        assert!(!page.handle_quit(QuitAction::Confirm));
        assert!(!page.handle_quit(QuitAction::Request));
        assert_eq!(page, IslandPage::ConfirmQuit);
        assert!(!page.handle_quit(QuitAction::Request));
        assert!(page.handle_quit(QuitAction::Confirm));
        assert_eq!(page, IslandPage::Controls);
        assert!(!page.handle_quit(QuitAction::Confirm));
    }

    #[test]
    fn cancel_or_disclosure_dismissal_invalidates_a_pending_quit() {
        let mut page = IslandPage::default();
        for _ in 0..3 {
            assert!(!page.handle_quit(QuitAction::Request));
            assert!(!page.handle_quit(QuitAction::Cancel));
            assert_eq!(page, IslandPage::Controls);
            assert!(!page.handle_quit(QuitAction::Confirm));
        }
    }

    #[test]
    fn quit_confirmation_fits_the_island_and_keeps_repeat_clicks_on_cancel() {
        let confirm_left = -QUIT_CONFIRM_ROW_WIDTH / 2.0 + QUIT_PROMPT_WIDTH + QUIT_CONFIRM_GAP;
        let confirm_right = confirm_left + QUIT_CONFIRM_WIDTH;
        let cancel_left = confirm_right + QUIT_CONFIRM_GAP;
        let cancel_right = cancel_left + QUIT_CANCEL_WIDTH;
        let old_quit_center =
            IDLE_CAPTURE_CENTER_X + 2.0 * (ACTION_CONTROL_SIZE + EXPANDED_CONTROL_GAP);
        assert!(old_quit_center > confirm_right);
        assert!(old_quit_center >= cancel_left && old_quit_center <= cancel_right);
        const {
            assert!(
                QUIT_CONFIRM_ROW_WIDTH
                    <= EXPANDED_WIDTH - 2.0 * crate::overlay::ISLAND_SHOULDER_INSET
            );
        }
        assert_eq!(cancel_right, QUIT_CONFIRM_ROW_WIDTH / 2.0);
    }

    fn contrast_ratio(left: u32, right: u32) -> f32 {
        let lighter = relative_luminance(left).max(relative_luminance(right));
        let darker = relative_luminance(left).min(relative_luminance(right));
        (lighter + 0.05) / (darker + 0.05)
    }

    #[test]
    fn titlebar_palette_preserves_the_sampled_surface() {
        for surface in [0x111111, 0xf3f3f3] {
            assert_eq!(TitlebarPalette::for_surface(surface).surface, surface);
        }
    }

    #[test]
    fn expanded_control_hints_separate_actions_from_their_details() {
        assert_eq!(
            ExpandedControl::Capture.hover_hint(false),
            ControlHint {
                label: "撮影",
                detail: "Ctrl+Alt+S",
                is_shortcut: true,
            }
        );
        assert_eq!(
            ExpandedControl::Save.hover_hint(false),
            ControlHint {
                label: "PNG保存",
                detail: "OFF",
                is_shortcut: false,
            }
        );
        assert_eq!(ExpandedControl::Save.hover_hint(true).detail, "ON");
        assert_eq!(
            ExpandedControl::Refresh.hover_hint(false),
            ControlHint {
                label: "再検出",
                detail: "会議・共有",
                is_shortcut: false,
            }
        );
        assert_eq!(
            ExpandedControl::Quit.hover_hint(false),
            ControlHint {
                label: "終了",
                detail: "Snapbar",
                is_shortcut: false,
            }
        );
    }

    #[test]
    fn idle_camera_stays_neutral_until_hover_disclosure() {
        for surface in [0x111111, 0xf3f3f3] {
            let palette = TitlebarPalette::for_surface(surface);
            assert_eq!(idle_camera_color(palette, true), palette.primary_text);
            assert_eq!(idle_camera_color(palette, false), palette.secondary_text);
            assert_ne!(idle_camera_color(palette, true), 0xe5484d);
        }
    }

    #[test]
    fn titlebar_palette_inverts_text_and_controls_for_light_and_dark() {
        let dark = TitlebarPalette::for_surface(0x111111);
        let light = TitlebarPalette::for_surface(0xf3f3f3);
        assert!(!dark.is_light);
        assert!(light.is_light);
        assert!(relative_luminance(dark.primary_text) > relative_luminance(dark.surface));
        assert!(relative_luminance(light.primary_text) < relative_luminance(light.surface));
        assert!(relative_luminance(dark.control_hover) > relative_luminance(dark.surface));
        assert!(relative_luminance(light.control_hover) < relative_luminance(light.surface));
    }

    #[test]
    fn titlebar_text_contrast_remains_accessible_in_both_themes() {
        for surface in [0x111111, 0x242424, 0xeeeeee, 0xf5f5f5] {
            let palette = TitlebarPalette::for_surface(surface);
            assert!(contrast_ratio(palette.primary_text, palette.surface) >= 4.5);
            assert!(contrast_ratio(palette.secondary_text, palette.surface) >= 4.5);
        }
    }

    #[test]
    fn presenter_surface_is_visible_before_hover_while_titlebar_surface_stays_transparent() {
        assert_eq!(disclosure_surface_alpha(true, 0.0), 1.0);
        assert_eq!(disclosure_surface_alpha(true, 1.0), 1.0);
        assert_eq!(disclosure_surface_alpha(false, 0.0), 1.0 / 255.0);
        assert_eq!(
            disclosure_surface_alpha(false, TITLEBAR_SURFACE_REVEAL_END),
            1.0
        );
    }

    #[test]
    fn capture_control_keeps_the_idle_x_coordinate_when_expanded() {
        let expanded_capture_center =
            EXPANDED_CAPTURE_CENTER_X_UNSHIFTED + EXPANDED_CONTENT_SHIFT_X;
        assert_eq!(expanded_capture_center, IDLE_CAPTURE_CENTER_X);
    }

    #[test]
    fn inline_disclosure_has_a_visible_showcase_phase_before_reaching_the_row() {
        let state = gpui::SpringState {
            position: 0.0,
            velocity: 0.0,
        };
        let spring = disclosure_spring_config(OverlayPresentation::InlineTitlebar, false);
        let position_after_100ms = spring.step(state, 1.0, 0.1);
        let position_after_180ms = spring.step(state, 1.0, 0.18);

        assert!((0.62..=0.78).contains(&position_after_100ms.position));
        assert!((0.98..=1.03).contains(&position_after_180ms.position));
    }

    #[test]
    fn disclosure_retarget_keeps_position_but_starts_a_fresh_spring_generation() {
        let mut animation = DisclosureAnimationState::default();
        animation.retarget(0.63, OverlayPresentation::HoverIsland);

        assert_eq!(animation.generation, 1);
        assert_eq!(animation.start, 0.63);

        animation.retarget(0.41, OverlayPresentation::HoverIsland);
        assert_eq!(animation.generation, 2);
        assert_eq!(animation.start, 0.41);
    }

    #[test]
    fn repeated_hover_reversals_do_not_amplify_the_visible_bounce() {
        for presentation in [
            OverlayPresentation::HoverIsland,
            OverlayPresentation::InlineTitlebar,
        ] {
            for spring_position in [0.0, 0.35, 0.92, 1.0, 1.008, 1.02, 1.04] {
                let initial_progress =
                    disclosure_presentation_progress(spring_position, presentation);
                let initial_width =
                    disclosure_width_for_attachment(initial_progress, presentation, false);
                let mut progress = initial_progress;
                let mut animation = DisclosureAnimationState::default();
                for _ in 0..20 {
                    animation.retarget(progress, presentation);
                    progress = disclosure_presentation_progress(animation.start, presentation);
                    assert!(
                        (disclosure_width_for_attachment(progress, presentation, false)
                            - initial_width)
                            .abs()
                            < 0.001,
                        "a direction change must not jump the first visible frame"
                    );
                }
            }
        }
    }

    #[test]
    fn caption_morph_keeps_the_camera_under_the_original_pointer_throughout_disclosure() {
        let idle_y = TITLEBAR_SURFACE_HEIGHT / 2.0;
        let mut previous_y = idle_y;
        for step in 0..=1_000 {
            let progress = step as f32 / 1_000.0;
            let motion = island_content_motion(progress);
            let hit_height = TITLEBAR_SURFACE_HEIGHT
                + (ACTION_CONTROL_SIZE - TITLEBAR_SURFACE_HEIGHT) * motion.capture_form;
            assert!(motion.center_y >= previous_y);
            assert!(idle_y >= motion.center_y - hit_height / 2.0);
            assert!(idle_y <= motion.center_y + hit_height / 2.0);
            assert!(motion.center_y - 8.5 >= 0.0);
            assert!(motion.center_y + 8.5 <= disclosure_height(progress));
            previous_y = motion.center_y;
        }
        assert_eq!(previous_y, HOVER_ISLAND_HEIGHT / 2.0);
    }

    #[test]
    fn caption_controls_follow_the_surface_without_restarting_on_reversal() {
        for progress in [0.0, 0.06, 0.12, 0.22] {
            let motion = island_content_motion(progress);
            assert_eq!(motion.details_alpha, 0.0);
        }
        // Reversing late in collapse must reproduce the camera and
        // contents of that frame without starting another independent animation.
        for progress in [0.1, 0.4, 0.6, 0.9, 1.02] {
            let before = island_content_motion(progress);
            let mut animation = DisclosureAnimationState::default();
            animation.retarget(progress, OverlayPresentation::HoverIsland);
            let after = island_content_motion(disclosure_presentation_progress(
                animation.start,
                OverlayPresentation::HoverIsland,
            ));
            assert!((before.center_y - after.center_y).abs() < 0.001);
            assert!((before.status_center_x - after.status_center_x).abs() < 0.001);
            assert!((before.status_center_y - after.status_center_y).abs() < 0.001);
            assert!((before.details_alpha - after.details_alpha).abs() < 0.001);
            assert!((before.auxiliary_alpha - after.auxiliary_alpha).abs() < 0.001);
            assert!((before.auxiliary_inset - after.auxiliary_inset).abs() < 0.001);
        }
        let expanded = island_content_motion(1.0);
        assert_eq!(expanded.capture_form, 1.0);
        assert_eq!(expanded.details_alpha, 1.0);
    }

    #[test]
    fn persistent_status_stays_inside_the_surface_and_clear_of_the_emerging_label() {
        let mut previous_x = EXPANDED_STATUS_CENTER_X;
        for step in (0..=1_000).rev() {
            let progress = step as f32 / 1_000.0;
            let motion = island_content_motion(progress);
            let half_width =
                disclosure_width_for_attachment(progress, OverlayPresentation::HoverIsland, false)
                    / 2.0;
            assert!(motion.status_center_x - STATUS_INDICATOR_SIZE / 2.0 >= -half_width);
            assert!(motion.status_center_x + STATUS_INDICATOR_SIZE / 2.0 <= half_width);
            assert!(motion.status_center_y - STATUS_INDICATOR_SIZE / 2.0 >= 0.0);
            // The moving status dot stays above the shaped shoulder and cannot
            // be clipped by a shrinking native region at any progress, including
            // the former all-transparent gap between 0.38 and 0.48.
            assert!(motion.status_center_y + STATUS_INDICATOR_SIZE / 2.0 < TITLEBAR_SURFACE_HEIGHT);
            // Every point must lie on the segment joining the two resting
            // positions, including during interrupted expansion/collapse.
            let horizontal = (motion.status_center_x - IDLE_STATUS_CENTER_X)
                / (EXPANDED_STATUS_CENTER_X - IDLE_STATUS_CENTER_X);
            let vertical = (motion.status_center_y - TITLEBAR_SURFACE_HEIGHT / 2.0)
                / (HOVER_ISLAND_HEIGHT / 2.0 + HOVER_CONTROLS_OFFSET_Y
                    - TITLEBAR_SURFACE_HEIGHT / 2.0);
            assert!((horizontal - vertical).abs() < 0.00001);
            assert!((motion.status_center_x - previous_x).abs() < 1.0);
            if motion.details_alpha > 0.0 {
                assert_eq!(motion.status_center_x, EXPANDED_STATUS_CENTER_X);
            }
            previous_x = motion.status_center_x;
        }
        assert_eq!(previous_x, IDLE_STATUS_CENTER_X);
    }

    #[test]
    fn auxiliary_icons_converge_inside_the_surface_until_their_fade_finishes() {
        let pitch = ACTION_CONTROL_SIZE + EXPANDED_CONTROL_GAP;
        let mut previous_alpha = 1.0;
        let mut previous_inset = 0.0;
        for step in (0..=1_000).rev() {
            let progress = step as f32 / 1_000.0;
            let motion = island_content_motion(progress);
            assert!(motion.auxiliary_alpha <= previous_alpha);
            assert!(motion.auxiliary_inset >= previous_inset);
            previous_alpha = motion.auxiliary_alpha;
            previous_inset = motion.auxiliary_inset;
            if motion.auxiliary_alpha == 0.0 {
                continue;
            }
            let half_width =
                disclosure_width_for_attachment(progress, OverlayPresentation::HoverIsland, false)
                    / 2.0;
            for center_x in [
                IDLE_CAPTURE_CENTER_X - pitch + motion.auxiliary_inset,
                IDLE_CAPTURE_CENTER_X + pitch - motion.auxiliary_inset,
                IDLE_CAPTURE_CENTER_X + 2.0 * (pitch - motion.auxiliary_inset),
            ] {
                assert!(center_x - ACTION_VISUAL_SIZE / 2.0 >= -half_width);
                assert!(center_x + ACTION_VISUAL_SIZE / 2.0 <= half_width);
            }
            assert!(motion.center_y - ACTION_VISUAL_SIZE / 2.0 >= 0.0);
            assert!(motion.center_y + ACTION_VISUAL_SIZE / 2.0 <= disclosure_height(progress));
        }
        assert_eq!(previous_alpha, 0.0);
        assert_eq!(island_content_motion(1.0).auxiliary_inset, 0.0);
    }

    #[test]
    fn collapse_keeps_icons_visible_while_they_return_to_the_caption() {
        let spring = disclosure_spring_config(OverlayPresentation::HoverIsland, true);
        let state = gpui::SpringState {
            position: 1.0,
            velocity: 0.0,
        };
        let middle = island_content_motion(spring.step(state, 0.0, 0.1).position);
        assert_eq!(middle.details_alpha, 0.0);
        assert!(middle.auxiliary_alpha > 0.1);
        assert!(middle.center_y < 18.0);
        assert!(middle.auxiliary_inset > 0.0);
        let finish = island_content_motion(spring.step(state, 0.0, 0.22).position);
        assert_eq!(finish.auxiliary_alpha, 0.0);
        assert_eq!(finish.center_y, TITLEBAR_SURFACE_HEIGHT / 2.0);
    }

    #[test]
    fn expanded_controls_begin_fanning_before_the_old_disclosure_threshold() {
        assert_eq!(expanded_control_gap(0.0), 0.0);
        assert!(expanded_control_gap(0.42) > 0.0);
        assert!(smoothstep_between(0.16, 0.86, 0.42) > 0.0);
    }

    #[test]
    fn inline_disclosure_keeps_a_bounded_elastic_settle() {
        assert_eq!(inline_disclosure_progress(1.0), 1.0);
        assert!(inline_disclosure_progress(1.04) < 1.0);
        assert!(inline_disclosure_progress(1.04) > 0.96);
        assert!(inline_disclosure_progress(1.2) <= 1.0);
    }

    #[test]
    fn expanded_controls_fan_out_without_moving_the_capture_anchor() {
        let capture_center = |gap: f32| {
            let controls_width = CONTEXT_PANEL_WIDTH + ACTION_CONTROL_SIZE * 4.0 + gap * 4.0;
            -controls_width / 2.0
                + CONTEXT_PANEL_WIDTH
                + gap
                + ACTION_CONTROL_SIZE
                + gap
                + ACTION_CONTROL_SIZE / 2.0
        };

        assert_eq!(expanded_control_gap(0.0), 0.0);
        assert_eq!(expanded_control_gap(1.0), EXPANDED_CONTROL_GAP);
        assert_eq!(capture_center(0.0), capture_center(EXPANDED_CONTROL_GAP));
    }

    #[test]
    fn expanded_controls_keep_the_added_edge_gutters() {
        let controls_left = -EXPANDED_CONTROLS_WIDTH / 2.0 + EXPANDED_CONTENT_SHIFT_X;
        let controls_right = controls_left + EXPANDED_CONTROLS_WIDTH;
        let left_gutter = controls_left + EXPANDED_WIDTH / 2.0;
        let right_gutter = EXPANDED_WIDTH / 2.0 - controls_right;

        assert!(left_gutter >= MIN_EXPANDED_EDGE_GUTTER);
        assert!(right_gutter >= MIN_EXPANDED_EDGE_GUTTER);
    }

    #[test]
    fn expanded_controls_gain_visual_padding_without_shrinking_hit_targets() {
        assert_eq!(ACTION_CONTROL_SIZE, 30.0);
        assert_eq!(ACTION_VISUAL_SIZE, 28.0);
        assert_eq!(CONTEXT_PANEL_WIDTH, 82.0);
        assert_eq!(CONTEXT_PANEL_VISUAL_WIDTH, 80.0);
        assert_eq!(EXPANDED_CONTROL_GAP + CONTROL_VISUAL_INSET * 2.0, 8.0);

        let two_row_content_height = CONTEXT_LABEL_LINE_HEIGHT
            + CONTEXT_ROW_GAP
            + CONTEXT_DETAIL_LINE_HEIGHT
            + CONTEXT_SHORTCUT_VERTICAL_PADDING * 2.0;
        assert!(two_row_content_height <= ACTION_VISUAL_SIZE);

        let visual_top = (HOVER_ISLAND_HEIGHT - ACTION_CONTROL_SIZE) / 2.0
            + CONTROL_VISUAL_INSET
            + HOVER_CONTROLS_OFFSET_Y;
        let visual_bottom_margin = HOVER_ISLAND_HEIGHT - visual_top - ACTION_VISUAL_SIZE;
        assert_eq!(visual_top, 10.5);
        assert_eq!(visual_bottom_margin, visual_top);
    }
}
