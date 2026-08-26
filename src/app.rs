use std::time::Duration;

use crate::{
    assets::Assets,
    capture::{
        CaptureEngine, CaptureTarget, save_clipboard_image_to_screenshots, show_capture_flash,
    },
    meeting::{MeetingMonitor, MeetingSnapshot},
    overlay::{
        COLLAPSED_HEIGHT, COLLAPSED_WIDTH, COMPACT_HEIGHT, COMPACT_WIDTH, EXPANDED_HEIGHT,
        EXPANDED_WIDTH, TeamsWindowFollower, WINDOW_HEIGHT, WINDOW_WIDTH,
    },
    resident::ResidentController,
    settings::AppSettings,
};
use gpui::{
    App, Bounds, ClickEvent, Context, Task, Window, WindowBackgroundAppearance, WindowBounds,
    WindowDecorations, WindowKind, WindowOptions, div, prelude::*, px, rgb, size, svg,
    transparent_black,
};
use gpui_platform::application;

const RESIDENT_SYNC_INTERVAL: Duration = Duration::from_millis(250);
const EXPAND_DELAY: Duration = Duration::from_millis(180);
const COLLAPSE_DELAY: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureState {
    Idle,
    WaitingForShare,
    Capturing,
    Copied,
    NoTarget,
    Error,
}

struct Snapbar {
    targets: Vec<CaptureTarget>,
    selected_target: usize,
    capture_engine: Option<CaptureEngine>,
    follower: Option<TeamsWindowFollower>,
    meeting_monitor: MeetingMonitor,
    resident: ResidentController,
    last_monitor_generation: u64,
    shared_content_hint: bool,
    compact_layout: bool,
    settings: AppSettings,
    expanded: bool,
    surface_hovered: bool,
    hover_transition: Option<Task<()>>,
    capture_state: CaptureState,
    capture_generation: u64,
    last_error: Option<String>,
}

impl Snapbar {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut snapbar = Self {
            targets: Vec::new(),
            selected_target: 0,
            capture_engine: None,
            follower: TeamsWindowFollower::start(window),
            meeting_monitor: MeetingMonitor::start(),
            resident: ResidentController::start(),
            last_monitor_generation: u64::MAX,
            shared_content_hint: false,
            compact_layout: false,
            settings: AppSettings::load(),
            expanded: false,
            surface_hovered: false,
            hover_transition: None,
            capture_state: CaptureState::NoTarget,
            capture_generation: 0,
            last_error: None,
        };
        let snapshot = snapbar.meeting_monitor.snapshot();
        snapbar.apply_meeting_snapshot(snapshot, cx);
        snapbar.start_resident_sync(window, cx);
        snapbar
    }

    fn current_target(&self) -> Option<&CaptureTarget> {
        self.targets.get(self.selected_target)
    }

    fn sync_follower(&self) {
        if let Some(follower) = &self.follower {
            follower.set_target(self.current_target().map(|target| target.id));
        }
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

        let compact_layout = self
            .follower
            .as_ref()
            .is_some_and(TeamsWindowFollower::is_compact);
        if compact_layout != self.compact_layout {
            self.compact_layout = compact_layout;
            if compact_layout {
                self.reset_disclosure(cx);
            }
            changed = true;
        }

        let follower_visible = self
            .follower
            .as_ref()
            .is_some_and(TeamsWindowFollower::is_visible);
        if !follower_visible
            && (self.expanded || self.surface_hovered || self.hover_transition.is_some())
        {
            self.reset_disclosure(cx);
            changed = true;
        }

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
            None if self.current_target().is_none() => {
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
        let next_id = snapshot.target.as_ref().map(|target| target.id);
        let was_shared = self.shared_content_hint;
        self.shared_content_hint = snapshot.shared_content_hint;

        if previous_id != next_id {
            self.reset_disclosure(cx);
        }

        match snapshot.target {
            Some(target) => {
                self.targets = vec![target];
                self.selected_target = 0;
                self.sync_follower();

                if self.shared_content_hint {
                    if previous_id != next_id || !was_shared || self.capture_engine.is_none() {
                        self.restart_capture_engine();
                    }
                } else {
                    self.capture_generation = self.capture_generation.wrapping_add(1);
                    self.capture_engine = None;
                    self.capture_state = CaptureState::WaitingForShare;
                    self.last_error = None;
                }
            }
            None => {
                self.capture_generation = self.capture_generation.wrapping_add(1);
                self.capture_engine = None;
                self.targets.clear();
                self.selected_target = 0;
                self.capture_state = CaptureState::NoTarget;
                self.last_error = None;
                self.shared_content_hint = false;
                self.reset_disclosure(cx);
                self.sync_follower();
            }
        }
    }

    fn restart_capture_engine(&mut self) {
        self.capture_generation = self.capture_generation.wrapping_add(1);
        self.capture_engine = None;
        self.sync_follower();
        let Some(target_id) = self.current_target().map(|target| target.id) else {
            self.capture_state = CaptureState::NoTarget;
            self.last_error = None;
            return;
        };
        if !self.shared_content_hint {
            self.capture_state = CaptureState::WaitingForShare;
            self.last_error = None;
            return;
        }

        match CaptureEngine::start(target_id) {
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
        if self.current_target().is_some() && self.shared_content_hint {
            self.restart_capture_engine();
        }
    }

    fn reset_disclosure(&mut self, cx: &mut Context<Self>) {
        self.hover_transition = None;
        self.surface_hovered = false;
        self.set_expanded(false, cx);
    }

    fn set_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        let expanded = expanded && !self.compact_layout;
        if let Some(follower) = &self.follower {
            follower.set_expanded(expanded);
        }
        if self.expanded == expanded {
            return;
        }
        self.expanded = expanded;
        cx.notify();
    }

    fn on_surface_hovered(&mut self, hovered: &bool, window: &mut Window, cx: &mut Context<Self>) {
        self.hover_transition = None;

        if self.compact_layout {
            self.surface_hovered = false;
            self.set_expanded(false, cx);
            return;
        }

        self.surface_hovered = *hovered;
        let target_expanded = *hovered;
        if self.expanded == target_expanded {
            return;
        }

        let delay = if target_expanded {
            EXPAND_DELAY
        } else {
            COLLAPSE_DELAY
        };
        self.hover_transition = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                if this.compact_layout || this.surface_hovered != target_expanded {
                    return;
                }
                this.set_expanded(target_expanded, cx);
            });
        }));
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
            self.capture_state = if self.current_target().is_some() {
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

        self.capture_generation = self.capture_generation.wrapping_add(1);
        let generation = self.capture_generation;
        let save_to_screenshots = self.settings.save_to_screenshots;
        self.capture_state = CaptureState::Capturing;
        self.last_error = None;
        cx.notify();

        let task = cx.background_executor().spawn(async move {
            let receipt = engine.copy_latest_to_clipboard()?;
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
                        show_capture_flash(receipt.screen_rect);
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

    fn status_color(&self) -> gpui::Rgba {
        match self.capture_state {
            CaptureState::Idle | CaptureState::Copied => rgb(0x55c27c),
            CaptureState::Capturing => rgb(0xe5484d),
            CaptureState::WaitingForShare | CaptureState::NoTarget => rgb(0xe0a24a),
            CaptureState::Error => rgb(0xf07178),
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
        let save_to_screenshots = self.settings.save_to_screenshots;
        let status_color = self.status_color();
        let primary_text = rgb(0xf5f5f6);
        let secondary_text = rgb(0x9b9ba2);
        let capture_background = match self.capture_state {
            CaptureState::NoTarget | CaptureState::WaitingForShare => rgb(0x35353a),
            CaptureState::Capturing => rgb(0xc83f47),
            CaptureState::Error => rgb(0xd1444c),
            CaptureState::Idle | CaptureState::Copied => rgb(0xe5484d),
        };

        let compact_camera = div()
            .id("titlebar-surface")
            .flex()
            .items_center()
            .justify_center()
            .w(px(COMPACT_WIDTH))
            .h(px(COMPACT_HEIGHT))
            .rounded_full()
            .bg(rgb(0x0b0b0d))
            .shadow_sm()
            .child(
                div()
                    .id("compact-capture-button")
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(30.0))
                    .rounded_full()
                    .bg(capture_background)
                    .when(can_capture, |button| {
                        button
                            .cursor_pointer()
                            .hover(|button| button.opacity(0.91))
                            .active(|button| button.opacity(0.68))
                            .on_click(cx.listener(Self::on_capture_clicked))
                    })
                    .when(!can_capture, |button| button.opacity(0.66))
                    .child(svg().path("icons/camera.svg").size(px(16.0)).text_color(
                        if can_capture {
                            rgb(0xffffff)
                        } else {
                            rgb(0xa7a7ac)
                        },
                    )),
            );

        let collapsed = div()
            .id("titlebar-surface")
            .flex()
            .items_center()
            .justify_center()
            .gap(px(7.0))
            .w(px(COLLAPSED_WIDTH))
            .h(px(COLLAPSED_HEIGHT))
            .rounded_full()
            .bg(rgb(0x0b0b0d))
            .shadow_sm()
            .cursor_pointer()
            .on_hover(cx.listener(Self::on_surface_hovered))
            .child(div().size(px(7.0)).rounded_full().bg(status_color))
            .child(
                svg()
                    .path("icons/camera.svg")
                    .size(px(15.0))
                    .text_color(if can_capture {
                        primary_text
                    } else {
                        secondary_text
                    }),
            )
            .child(div().text_xs().text_color(primary_text).child("Snapbar"));

        let status = div()
            .id("status-button")
            .flex()
            .items_center()
            .gap(px(7.0))
            .w(px(82.0))
            .h(px(28.0))
            .px(px(8.0))
            .rounded(px(11.0))
            .cursor_pointer()
            .text_xs()
            .text_color(primary_text)
            .hover(|button| button.bg(rgb(0x1a1a1e)))
            .active(|button| button.opacity(0.72))
            .on_click(cx.listener(Self::on_refresh_clicked))
            .child(div().size(px(7.0)).rounded_full().bg(status_color))
            .child(self.status_label());

        let capture = div()
            .id("capture-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(32.0))
            .rounded_full()
            .bg(capture_background)
            .shadow_sm()
            .when(can_capture, |button| {
                button
                    .cursor_pointer()
                    .hover(|button| button.opacity(0.91))
                    .active(|button| button.opacity(0.68))
                    .on_click(cx.listener(Self::on_capture_clicked))
            })
            .when(!can_capture, |button| button.opacity(0.66))
            .child(
                svg()
                    .path("icons/camera.svg")
                    .size(px(17.0))
                    .text_color(if can_capture {
                        rgb(0xffffff)
                    } else {
                        rgb(0xa7a7ac)
                    }),
            );

        let save = div()
            .id("save-toggle")
            .flex()
            .items_center()
            .justify_center()
            .size(px(30.0))
            .rounded(px(10.0))
            .cursor_pointer()
            .bg(if save_to_screenshots {
                rgb(0x285d40)
            } else {
                rgb(0x18181b)
            })
            .hover(|button| {
                button.bg(if save_to_screenshots {
                    rgb(0x347451)
                } else {
                    rgb(0x28282d)
                })
            })
            .active(|button| button.opacity(0.72))
            .on_click(cx.listener(Self::on_save_toggle_clicked))
            .child(svg().path("icons/folder.svg").size(px(16.0)).text_color(
                if save_to_screenshots {
                    rgb(0xe8fff0)
                } else {
                    rgb(0xc8c8cd)
                },
            ));

        let refresh = div()
            .id("refresh-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(30.0))
            .rounded(px(10.0))
            .cursor_pointer()
            .bg(rgb(0x18181b))
            .hover(|button| button.bg(rgb(0x28282d)))
            .active(|button| button.opacity(0.72))
            .on_click(cx.listener(Self::on_refresh_clicked))
            .child(
                svg()
                    .path("icons/refresh.svg")
                    .size(px(16.0))
                    .text_color(rgb(0xc8c8cd)),
            );

        let quit = div()
            .id("quit-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(30.0))
            .rounded(px(10.0))
            .cursor_pointer()
            .bg(rgb(0x18181b))
            .hover(|button| button.bg(rgb(0x3a2025)))
            .active(|button| button.opacity(0.72))
            .on_click(|_, _, cx| cx.quit())
            .child(
                svg()
                    .path("icons/power.svg")
                    .size(px(16.0))
                    .text_color(rgb(0xf07178)),
            );

        let expanded = div()
            .id("titlebar-surface")
            .flex()
            .items_center()
            .justify_center()
            .gap(px(6.0))
            .w(px(EXPANDED_WIDTH))
            .h(px(EXPANDED_HEIGHT))
            .px(px(7.0))
            .rounded_full()
            .bg(rgb(0x0b0b0d))
            .shadow_lg()
            .on_hover(cx.listener(Self::on_surface_hovered))
            .child(status)
            .child(capture)
            .child(save)
            .child(refresh)
            .child(quit);

        let surface = if self.compact_layout {
            compact_camera
        } else if self.expanded {
            expanded
        } else {
            collapsed
        };

        div()
            .id("titlebar-overlay-root")
            .flex()
            .items_center()
            .justify_center()
            .size_full()
            .bg(transparent_black())
            .child(surface)
    }
}

pub fn run() {
    application().with_assets(Assets).run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                focus: false,
                kind: WindowKind::PopUp,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                window_background: WindowBackgroundAppearance::Transparent,
                window_decorations: Some(WindowDecorations::Client),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Snapbar::new(window, cx)),
        )
        .expect("Snapbar window could not be created");

        cx.activate(true);
    });
}
