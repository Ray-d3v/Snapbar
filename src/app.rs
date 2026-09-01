use std::{thread, time::Duration};

use crate::{
    assets::Assets,
    capture::{
        CaptureEngine, CaptureSource, CaptureTarget, LocalMonitorCaptureTarget,
        save_clipboard_image_to_screenshots, show_capture_flash,
    },
    meeting::{MeetingMonitor, MeetingSnapshot},
    overlay::{
        COLLAPSED_WIDTH, COMPACT_WIDTH, DEFAULT_TITLEBAR_COLOR, EXPANDED_HEIGHT, EXPANDED_WIDTH,
        HOVER_ISLAND_HEIGHT, INLINE_HEIGHT, INLINE_WIDTH, OverlayCaptureMode, OverlayPresentation,
        TITLEBAR_SURFACE_HEIGHT, TeamsWindowFollower, WINDOW_HEIGHT, WINDOW_WIDTH,
        disclosure_height, disclosure_width, presenter_disclosure_height,
    },
    resident::ResidentController,
    settings::AppSettings,
};
use gpui::{
    AnimationExt as _, App, Bounds, ClickEvent, Context, SpringAnimation, SpringConfig, Window,
    WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowKind, WindowOptions, div,
    prelude::*, px, relative, rgb, rgba, size, svg, transparent_black,
};
use gpui_platform::application;

const RESIDENT_SYNC_INTERVAL: Duration = Duration::from_millis(250);
const RECORDABLE_OVERLAY_EXCLUSION_SETTLE: Duration = Duration::from_millis(34);
const DISCLOSURE_SPRING_STIFFNESS: f32 = 900.0;
const DISCLOSURE_SPRING_DAMPING: f32 = 46.0;
const INLINE_DISCLOSURE_SPRING_STIFFNESS: f32 = 360.0;
const INLINE_DISCLOSURE_SPRING_DAMPING: f32 = 28.0;
const INLINE_DISCLOSURE_BOUNCE_RESTITUTION: f32 = 0.55;
const DISCLOSURE_OVERSHOOT_GAIN: f32 = 2.0;
const DISCLOSURE_MAX_PRESENTATION: f32 = 1.044;
const TITLEBAR_SURFACE_REVEAL_END: f32 = 0.10;
const EXPANDED_CONTROL_GAP: f32 = 6.0;
const STATUS_CONTROL_WIDTH: f32 = 82.0;
const ACTION_CONTROL_SIZE: f32 = 30.0;
const CONTROL_VISUAL_INSET: f32 = 1.0;
const STATUS_VISUAL_WIDTH: f32 = STATUS_CONTROL_WIDTH - CONTROL_VISUAL_INSET * 2.0;
const ACTION_VISUAL_SIZE: f32 = ACTION_CONTROL_SIZE - CONTROL_VISUAL_INSET * 2.0;
const HOVER_CONTROLS_OFFSET_Y: f32 = -CONTROL_VISUAL_INSET;
const PRESENTER_IDLE_CONTENT_OFFSET_Y: f32 = 5.0;
const PRESENTER_CONTROLS_OFFSET_Y: f32 = 0.0;
const EXPANDED_CONTROLS_WIDTH: f32 =
    STATUS_CONTROL_WIDTH + ACTION_CONTROL_SIZE * 4.0 + EXPANDED_CONTROL_GAP * 4.0;
const IDLE_CAPTURE_CENTER_X: f32 = -COLLAPSED_WIDTH / 2.0 + COMPACT_WIDTH + COMPACT_WIDTH / 2.0;
const EXPANDED_CAPTURE_CENTER_X_UNSHIFTED: f32 = -EXPANDED_CONTROLS_WIDTH / 2.0
    + STATUS_CONTROL_WIDTH
    + EXPANDED_CONTROL_GAP
    + ACTION_CONTROL_SIZE
    + EXPANDED_CONTROL_GAP
    + ACTION_CONTROL_SIZE / 2.0;
const EXPANDED_CONTENT_SHIFT_X: f32 = IDLE_CAPTURE_CENTER_X - EXPANDED_CAPTURE_CENTER_X_UNSHIFTED;

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

fn disclosure_spring_config(presentation: OverlayPresentation) -> SpringConfig {
    if presentation.is_inline() {
        SpringConfig::new(
            INLINE_DISCLOSURE_SPRING_STIFFNESS,
            INLINE_DISCLOSURE_SPRING_DAMPING,
            1.0,
        )
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

fn expanded_control_gap(content_progress: f32) -> f32 {
    EXPANDED_CONTROL_GAP * smoothstep_between(0.12, 0.94, content_progress)
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
    control_background: u32,
    control_hover: u32,
    status_hover: u32,
    quit_hover: u32,
    control_icon: u32,
    disabled_control: u32,
    disabled_icon: u32,
    save_background: u32,
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
                control_background: mix_rgb(surface, 0x000000, 0.08),
                control_hover: mix_rgb(surface, 0x000000, 0.16),
                status_hover: mix_rgb(surface, 0x000000, 0.06),
                quit_hover: mix_rgb(surface, 0xc42b1c, 0.14),
                control_icon: 0x4f5058,
                disabled_control: mix_rgb(surface, 0x000000, 0.12),
                disabled_icon: 0x66666f,
                save_background: mix_rgb(surface, 0x2e7d4a, 0.22),
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
                control_background: mix_rgb(surface, 0xffffff, 0.08),
                control_hover: mix_rgb(surface, 0xffffff, 0.16),
                status_hover: mix_rgb(surface, 0xffffff, 0.06),
                quit_hover: mix_rgb(surface, 0xd1444c, 0.22),
                control_icon: 0xc8c8cd,
                disabled_control: mix_rgb(surface, 0xffffff, 0.14),
                disabled_icon: 0xa7a7ac,
                save_background: mix_rgb(surface, 0x3a9b60, 0.42),
                save_hover: mix_rgb(surface, 0x3a9b60, 0.55),
                save_icon: 0xe8fff0,
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
    Copied,
    NoTarget,
    Error,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct DisclosureAnimationState {
    generation: u64,
    start: f32,
}

impl DisclosureAnimationState {
    fn retarget(&mut self, current_progress: f32) {
        // A new element id keeps the current visual position but intentionally
        // drops velocity inherited from the opposite hover direction.
        self.generation = self.generation.wrapping_add(1);
        self.start = current_progress.clamp(0.0, DISCLOSURE_MAX_PRESENTATION);
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
    titlebar_color: u32,
    settings: AppSettings,
    expanded: bool,
    disclosure_animation: DisclosureAnimationState,
    capture_state: CaptureState,
    capture_generation: u64,
    last_error: Option<String>,
}

impl Snapbar {
    fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        presentation: OverlayPresentation,
        capture_mode: OverlayCaptureMode,
    ) -> Self {
        let follower = TeamsWindowFollower::start(window, presentation, capture_mode);
        let overlay_events = follower.as_ref().map(TeamsWindowFollower::subscribe);
        let titlebar_color = follower
            .as_ref()
            .map(TeamsWindowFollower::titlebar_color)
            .unwrap_or(DEFAULT_TITLEBAR_COLOR);
        let mut snapbar = Self {
            presentation,
            capture_mode,
            targets: Vec::new(),
            selected_target: 0,
            capture_engine: None,
            follower,
            meeting_monitor: MeetingMonitor::start(),
            resident: ResidentController::start(),
            last_monitor_generation: u64::MAX,
            shared_content_hint: false,
            presenter_toolbar_id: None,
            local_share_active: false,
            local_monitor_target: None,
            compact_layout: false,
            titlebar_color,
            settings: AppSettings::load(),
            expanded: false,
            disclosure_animation: DisclosureAnimationState::default(),
            capture_state: CaptureState::NoTarget,
            capture_generation: 0,
            last_error: None,
        };
        let snapshot = snapbar.meeting_monitor.snapshot();
        snapbar.apply_meeting_snapshot(snapshot, cx);
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
        if self.expanded == expanded {
            return false;
        }

        self.expanded = expanded;
        self.disclosure_animation.retarget(current_progress);
        true
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
            cx.quit();
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

        let next_source = self.current_capture_source();
        if next_source.is_some()
            && (previous_source != next_source || self.capture_engine.is_none())
        {
            self.restart_capture_engine();
        } else if next_source.is_none() {
            self.capture_generation = self.capture_generation.wrapping_add(1);
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
        let titlebar_color = follower.titlebar_color();
        let disclosure_progress = follower.disclosure_progress();
        let changed = self.compact_layout != compact
            || self.expanded != expanded
            || self.titlebar_color != titlebar_color;
        self.compact_layout = compact;
        self.retarget_disclosure(expanded, disclosure_progress);
        self.titlebar_color = titlebar_color;
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
        cx.notify();
    }

    fn on_capture_clicked(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.capture_state == CaptureState::Capturing {
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
        let save_to_screenshots = self.settings.save_to_screenshots;
        self.capture_state = CaptureState::Capturing;
        self.last_error = None;
        cx.notify();
        let wait_for_overlay_exclusion = local_monitor_capture
            && self.capture_mode == OverlayCaptureMode::Recordable
            && overlay_exclusion.is_some();

        let task = cx.background_executor().spawn(async move {
            let overlay_exclusion = overlay_exclusion;
            if wait_for_overlay_exclusion {
                thread::sleep(RECORDABLE_OVERLAY_EXCLUSION_SETTLE);
            }
            let receipt = engine.copy_latest_to_clipboard()?;
            drop(overlay_exclusion);
            let save_result = save_to_screenshots.then(save_clipboard_image_to_screenshots);
            Ok::<_, anyhow::Error>((receipt, save_result))
        });

        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.capture_generation != generation {
                    return;
                }

                match result {
                    Ok((receipt, save_result)) => {
                        show_capture_flash(
                            receipt.screen_rect,
                            this.capture_mode.display_affinity(),
                        );
                        let _ = receipt.frame_age;
                        let _ = receipt.latency;
                        this.capture_state = CaptureState::Copied;
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
            CaptureState::Copied => "コピー済",
            CaptureState::NoTarget => "会議待ち",
            CaptureState::Error => "要確認",
        }
    }

    fn status_color(&self, light_surface: bool) -> gpui::Rgba {
        match (light_surface, self.capture_state) {
            (true, CaptureState::Idle | CaptureState::Copied) => rgb(0x277a46),
            (true, CaptureState::Capturing) => rgb(0xc7363f),
            (true, CaptureState::WaitingForShare | CaptureState::NoTarget) => rgb(0x986500),
            (true, CaptureState::Error) => rgb(0xb4232c),
            (false, CaptureState::Idle | CaptureState::Copied) => rgb(0x55c27c),
            (false, CaptureState::Capturing) => rgb(0xe5484d),
            (false, CaptureState::WaitingForShare | CaptureState::NoTarget) => rgb(0xe0a24a),
            (false, CaptureState::Error) => rgb(0xf07178),
        }
    }
}

impl Render for Snapbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_capture = self
            .capture_engine
            .as_ref()
            .is_some_and(CaptureEngine::is_ready)
            && self.capture_state != CaptureState::Capturing;
        let presentation = self.presentation;
        let presenter_attached = self.presenter_toolbar_id.is_some();
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
        let palette = TitlebarPalette::for_surface(self.titlebar_color);
        let status_color = self.status_color(palette.is_light);
        let primary_text = rgb(palette.primary_text);
        let island_background = rgb(palette.surface);
        // A non-zero alpha keeps the layered HWND interactive while remaining visually
        // indistinguishable from the Teams title bar at rest.
        let idle_hit_surface = rgba(0x00000001);
        let idle_active_backplate = rgba(palette.idle_active_backplate);
        let capture_background = match self.capture_state {
            CaptureState::NoTarget | CaptureState::WaitingForShare => rgb(palette.disabled_control),
            CaptureState::Capturing => rgb(0xc83f47),
            CaptureState::Error => rgb(0xd1444c),
            CaptureState::Idle | CaptureState::Copied => rgb(0xe5484d),
        };
        let idle_camera_icon = || {
            svg()
                .path("icons/camera.svg")
                .size(px(16.0))
                .text_color(rgb(idle_camera_color(palette, can_capture)))
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
                    .child(div().size(px(6.0)).rounded_full().bg(status_color)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(COMPACT_WIDTH))
                    .h_full()
                    .bg(idle_hit_surface)
                    .cursor_pointer()
                    .child(idle_camera_icon()),
            );

        let status = div()
            .id("status-button")
            .flex()
            .items_center()
            .justify_center()
            .w(px(STATUS_CONTROL_WIDTH))
            .h(px(ACTION_CONTROL_SIZE))
            .cursor_pointer()
            .text_xs()
            .text_color(primary_text)
            .active(|button| button.opacity(0.72))
            .on_click(cx.listener(Self::on_refresh_clicked))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .w(px(STATUS_VISUAL_WIDTH))
                    .h(px(ACTION_VISUAL_SIZE))
                    .px(px(8.0))
                    .rounded(px(11.0))
                    .hover(move |button| button.bg(rgb(palette.status_hover)))
                    .child(div().size(px(7.0)).rounded_full().bg(status_color))
                    .child(self.status_label()),
            );

        let capture = div()
            .id("capture-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(ACTION_CONTROL_SIZE))
            .when(can_capture, |button| {
                button
                    .cursor_pointer()
                    .hover(|button| button.opacity(0.91))
                    .active(|button| button.opacity(0.68))
                    .on_click(cx.listener(Self::on_capture_clicked))
            })
            .when(!can_capture, |button| button.opacity(0.66))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(ACTION_VISUAL_SIZE))
                    .rounded_full()
                    .bg(capture_background)
                    .shadow_sm()
                    .child(svg().path("icons/camera.svg").size(px(17.0)).text_color(
                        if can_capture {
                            rgb(0xffffff)
                        } else {
                            rgb(palette.disabled_icon)
                        },
                    )),
            );

        let save = div()
            .id("save-toggle")
            .flex()
            .items_center()
            .justify_center()
            .size(px(ACTION_CONTROL_SIZE))
            .cursor_pointer()
            .active(|button| button.opacity(0.72))
            .on_click(cx.listener(Self::on_save_toggle_clicked))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(ACTION_VISUAL_SIZE))
                    .rounded(px(9.0))
                    .bg(if save_to_screenshots {
                        rgb(palette.save_background)
                    } else {
                        rgb(palette.control_background)
                    })
                    .hover(move |button| {
                        button.bg(if save_to_screenshots {
                            rgb(palette.save_hover)
                        } else {
                            rgb(palette.control_hover)
                        })
                    })
                    .child(svg().path("icons/folder.svg").size(px(16.0)).text_color(
                        if save_to_screenshots {
                            rgb(palette.save_icon)
                        } else {
                            rgb(palette.control_icon)
                        },
                    )),
            );

        let refresh = div()
            .id("refresh-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(ACTION_CONTROL_SIZE))
            .cursor_pointer()
            .active(|button| button.opacity(0.72))
            .on_click(cx.listener(Self::on_refresh_clicked))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(ACTION_VISUAL_SIZE))
                    .rounded(px(9.0))
                    .bg(rgb(palette.control_background))
                    .hover(move |button| button.bg(rgb(palette.control_hover)))
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
            .on_click(|_, _, cx| cx.quit())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(ACTION_VISUAL_SIZE))
                    .rounded(px(9.0))
                    .bg(rgb(palette.control_background))
                    .hover(move |button| button.bg(rgb(palette.quit_hover)))
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
            .px(px(7.0))
            .child(status)
            .child(save)
            .child(capture)
            .child(refresh)
            .child(quit);

        let disclosure_target = if self.expanded { 1.0 } else { 0.0 };
        let disclosure_start = self.disclosure_animation.start;
        let disclosure_generation = self.disclosure_animation.generation;
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
                SpringAnimation::new(disclosure_spring_config(presentation))
                    .to(disclosure_target)
                    .from(disclosure_start)
                    .with_epsilon(0.001),
                move |surface, spring_position| {
                    let spring_position = spring_position.max(0.0);
                    let progress = if presentation.is_inline() {
                        inline_disclosure_progress(spring_position)
                    } else if spring_position > 1.0 {
                        1.0 + (spring_position - 1.0) * DISCLOSURE_OVERSHOOT_GAIN
                    } else {
                        spring_position
                    }
                    .clamp(0.0, DISCLOSURE_MAX_PRESENTATION);
                    let content_progress = spring_position.clamp(0.0, 1.0);
                    if let Some(publisher) = &progress_publisher {
                        publisher.publish(progress);
                    }

                    let surface_width = disclosure_width(progress);
                    let surface_height = if presenter_attached {
                        presenter_disclosure_height(progress)
                    } else if presentation.is_inline() {
                        INLINE_HEIGHT
                    } else {
                        disclosure_height(progress)
                    };
                    let surface_alpha =
                        disclosure_surface_alpha(presenter_attached, content_progress);
                    let idle_alpha = 1.0 - smoothstep_between(0.12, 0.62, content_progress);
                    let expanded_alpha = smoothstep_between(0.16, 0.86, content_progress);
                    let control_gap = expanded_control_gap(content_progress);

                    surface
                        .child(
                            div()
                                .absolute()
                                .top(px(0.0))
                                .left(relative(0.5))
                                .ml(px(-surface_width / 2.0))
                                .w(px(surface_width))
                                .h(px(surface_height))
                                // SetWindowRgn supplies either the caption-attached island or
                                // the presenter-toolbar rounded rectangle and clips this fill.
                                .bg(island_background.opacity(surface_alpha)),
                        )
                        .when(content_progress < 0.72, |surface| {
                            surface.child(idle_content.opacity(idle_alpha))
                        })
                        .when(content_progress > 0.12, |surface| {
                            surface.child(
                                expanded_content
                                    .gap(px(control_gap))
                                    .opacity(expanded_alpha),
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
        assert!(
            relative_luminance(dark.control_hover) > relative_luminance(dark.control_background)
        );
        assert!(
            relative_luminance(light.control_hover) < relative_luminance(light.control_background)
        );
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
        let spring = disclosure_spring_config(OverlayPresentation::InlineTitlebar);
        let position_after_100ms = spring.step(state, 1.0, 0.1);
        let position_after_180ms = spring.step(state, 1.0, 0.18);

        assert!((0.62..=0.78).contains(&position_after_100ms.position));
        assert!((0.98..=1.03).contains(&position_after_180ms.position));
    }

    #[test]
    fn disclosure_retarget_keeps_position_but_starts_a_fresh_spring_generation() {
        let mut animation = DisclosureAnimationState::default();
        animation.retarget(0.63);

        assert_eq!(animation.generation, 1);
        assert_eq!(animation.start, 0.63);

        animation.retarget(0.41);
        assert_eq!(animation.generation, 2);
        assert_eq!(animation.start, 0.41);
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
            let controls_width = STATUS_CONTROL_WIDTH + ACTION_CONTROL_SIZE * 4.0 + gap * 4.0;
            -controls_width / 2.0
                + STATUS_CONTROL_WIDTH
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
    fn reordered_expanded_controls_stay_inside_the_island() {
        let controls_left = -EXPANDED_CONTROLS_WIDTH / 2.0 + EXPANDED_CONTENT_SHIFT_X;
        let controls_right = controls_left + EXPANDED_CONTROLS_WIDTH;
        assert!(controls_left >= -EXPANDED_WIDTH / 2.0);
        assert!(controls_right <= EXPANDED_WIDTH / 2.0);
    }

    #[test]
    fn expanded_controls_gain_visual_padding_without_shrinking_hit_targets() {
        assert_eq!(ACTION_CONTROL_SIZE, 30.0);
        assert_eq!(ACTION_VISUAL_SIZE, 28.0);
        assert_eq!(STATUS_CONTROL_WIDTH, 82.0);
        assert_eq!(STATUS_VISUAL_WIDTH, 80.0);
        assert_eq!(EXPANDED_CONTROL_GAP + CONTROL_VISUAL_INSET * 2.0, 8.0);

        let visual_top = (HOVER_ISLAND_HEIGHT - ACTION_CONTROL_SIZE) / 2.0
            + CONTROL_VISUAL_INSET
            + HOVER_CONTROLS_OFFSET_Y;
        let visual_bottom_margin = HOVER_ISLAND_HEIGHT - visual_top - ACTION_VISUAL_SIZE;
        assert_eq!(visual_top, 7.5);
        assert_eq!(visual_bottom_margin, 9.5);
    }
}
