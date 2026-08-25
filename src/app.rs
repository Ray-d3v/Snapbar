use std::time::Duration;

use crate::{
    assets::Assets,
    capture::{
        CaptureEngine, CaptureTarget, save_clipboard_image_to_screenshots, show_capture_flash,
    },
    meeting::{MeetingMonitor, MeetingSnapshot},
    overlay::TeamsWindowFollower,
    resident::ResidentController,
    settings::AppSettings,
};
use gpui::{
    App, Bounds, ClickEvent, Context, Window, WindowBackgroundAppearance, WindowBounds,
    WindowDecorations, WindowKind, WindowOptions, div, prelude::*, px, rgb, size, svg,
    transparent_black,
};
use gpui_platform::application;

const WINDOW_WIDTH: f32 = 286.0;
const EXPANDED_HEIGHT: f32 = 246.0;
const RESIDENT_SYNC_INTERVAL: Duration = Duration::from_millis(500);
const MENU_OPEN_DELAY: Duration = Duration::from_millis(110);
const MENU_CLOSE_DELAY: Duration = Duration::from_millis(220);

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
    settings: AppSettings,
    menu_open: bool,
    menu_pinned: bool,
    menu_button_hovered: bool,
    root_hovered: bool,
    menu_open_generation: u64,
    menu_close_generation: u64,
    capture_state: CaptureState,
    capture_count: u64,
    capture_generation: u64,
    last_latency_ms: Option<u128>,
    last_saved_path: Option<String>,
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
            settings: AppSettings::load(),
            menu_open: false,
            menu_pinned: false,
            menu_button_hovered: false,
            root_hovered: false,
            menu_open_generation: 0,
            menu_close_generation: 0,
            capture_state: CaptureState::NoTarget,
            capture_count: 0,
            capture_generation: 0,
            last_latency_ms: None,
            last_saved_path: None,
            last_error: None,
        };
        snapbar.apply_meeting_snapshot(snapbar.meeting_monitor.snapshot());
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
            self.apply_meeting_snapshot(snapshot);
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

    fn apply_meeting_snapshot(&mut self, snapshot: MeetingSnapshot) {
        self.last_monitor_generation = snapshot.generation;
        let previous_id = self.current_target().map(|target| target.id);
        let next_id = snapshot.target.as_ref().map(|target| target.id);
        let was_shared = self.shared_content_hint;
        self.shared_content_hint = snapshot.shared_content_hint;

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

    fn set_menu_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.menu_open == open {
            return;
        }
        self.menu_open = open;
        if let Some(follower) = &self.follower {
            follower.set_menu_open(open);
        }
        cx.notify();
    }

    fn cancel_menu_open(&mut self) {
        self.menu_open_generation = self.menu_open_generation.wrapping_add(1);
    }

    fn cancel_menu_close(&mut self) {
        self.menu_close_generation = self.menu_close_generation.wrapping_add(1);
    }

    fn schedule_menu_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu_open || self.menu_pinned {
            return;
        }
        self.cancel_menu_open();
        let generation = self.menu_open_generation;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(MENU_OPEN_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                if this.menu_open_generation == generation
                    && this.menu_button_hovered
                    && !this.menu_pinned
                {
                    this.set_menu_open(true, cx);
                }
            });
        })
        .detach();
    }

    fn schedule_menu_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu_pinned || !self.menu_open {
            return;
        }
        self.cancel_menu_close();
        let generation = self.menu_close_generation;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(MENU_CLOSE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                if this.menu_close_generation == generation
                    && !this.root_hovered
                    && !this.menu_pinned
                {
                    this.set_menu_open(false, cx);
                }
            });
        })
        .detach();
    }

    fn on_target_clicked(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.request_redetection();
        cx.notify();
    }

    fn on_refresh_clicked(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.request_redetection();
        cx.notify();
    }

    fn on_menu_clicked(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.cancel_menu_open();
        self.cancel_menu_close();
        self.menu_pinned = !self.menu_pinned;
        self.set_menu_open(self.menu_pinned, cx);
    }

    fn on_menu_hovered(&mut self, hovered: &bool, window: &mut Window, cx: &mut Context<Self>) {
        self.menu_button_hovered = *hovered;
        if *hovered {
            self.cancel_menu_close();
            self.schedule_menu_open(window, cx);
        } else {
            self.cancel_menu_open();
        }
    }

    fn on_root_hovered(&mut self, hovered: &bool, window: &mut Window, cx: &mut Context<Self>) {
        self.root_hovered = *hovered;
        if *hovered {
            self.cancel_menu_close();
        } else {
            self.cancel_menu_open();
            self.schedule_menu_close(window, cx);
        }
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
                        this.capture_count = this.capture_count.saturating_add(1);
                        this.capture_state = CaptureState::Copied;
                        this.last_latency_ms = Some(receipt.latency.as_millis());
                        this.last_saved_path = None;
                        this.last_error = None;

                        if let Some(save_result) = save_result {
                            match save_result {
                                Ok(path) => {
                                    this.last_saved_path = Some(path.display().to_string());
                                }
                                Err(error) => {
                                    this.last_error =
                                        Some(format!("コピー済み / ファイル保存失敗: {error}"));
                                }
                            }
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

    fn target_label(&self) -> String {
        if self.current_target().is_none() {
            return "会議待機中".to_string();
        }
        if self
            .capture_engine
            .as_ref()
            .is_some_and(CaptureEngine::is_ready)
        {
            "Teams".to_string()
        } else {
            "共有待ち".to_string()
        }
    }

    fn menu_summary(&self) -> String {
        if let Some(error) = &self.last_error {
            return truncate(error, 38);
        }
        if let Some(path) = &self.last_saved_path {
            return format!("保存: {}", truncate(path, 31));
        }
        let state = match self.capture_state {
            CaptureState::Idle => "準備完了",
            CaptureState::WaitingForShare => "共有待ち",
            CaptureState::Capturing => "撮影中",
            CaptureState::Copied if self.settings.save_to_screenshots => "コピー・保存済み",
            CaptureState::Copied => "コピー済み",
            CaptureState::NoTarget => "会議待機中",
            CaptureState::Error => "要確認",
        };
        let latency = self
            .last_latency_ms
            .map(|value| format!(" · {value}ms"))
            .unwrap_or_default();
        format!("{state} · {}枚{latency}", self.capture_count)
    }
}

impl Render for Snapbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_target = self.current_target().is_some();
        let can_capture = self
            .capture_engine
            .as_ref()
            .is_some_and(CaptureEngine::is_ready)
            && self.capture_state != CaptureState::Capturing;
        let menu_open = self.menu_open;
        let save_to_screenshots = self.settings.save_to_screenshots;
        let primary_text = rgb(0xf5f5f6);
        let secondary_text = rgb(0x96969d);
        let target_icon_color = match self.capture_state {
            CaptureState::Error => rgb(0xf07178),
            CaptureState::NoTarget | CaptureState::WaitingForShare => rgb(0xe0a24a),
            _ if has_target => rgb(0xd7d7dc),
            _ => secondary_text,
        };
        let capture_background = match self.capture_state {
            CaptureState::NoTarget | CaptureState::WaitingForShare => rgb(0x35353a),
            CaptureState::Capturing => rgb(0xc83f47),
            CaptureState::Error => rgb(0xd1444c),
            CaptureState::Idle | CaptureState::Copied => rgb(0xe5484d),
        };

        let target_button = div()
            .id("target-button")
            .flex()
            .items_center()
            .gap(px(9.0))
            .w(px(126.0))
            .h(px(44.0))
            .px(px(11.0))
            .rounded(px(15.0))
            .cursor_pointer()
            .text_sm()
            .text_color(if has_target {
                primary_text
            } else {
                secondary_text
            })
            .hover(|button| button.bg(rgb(0x18181b)))
            .active(|button| button.opacity(0.72))
            .on_click(cx.listener(Self::on_target_clicked))
            .child(
                svg()
                    .path("icons/window.svg")
                    .size(px(18.0))
                    .text_color(target_icon_color),
            )
            .child(self.target_label());

        let capture_button = div()
            .id("capture-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(46.0))
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
            .when(self.capture_state == CaptureState::Capturing, |button| {
                button.opacity(0.76)
            })
            .child(
                svg()
                    .path("icons/camera.svg")
                    .size(px(20.0))
                    .text_color(if can_capture {
                        rgb(0xffffff)
                    } else {
                        rgb(0xa7a7ac)
                    }),
            );

        let menu_icon_color = if menu_open {
            primary_text
        } else {
            rgb(0xc8c8ce)
        };
        let menu_button = div()
            .id("menu-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(42.0))
            .rounded(px(14.0))
            .cursor_pointer()
            .bg(if menu_open {
                rgb(0x242428)
            } else {
                rgb(0x17171a)
            })
            .hover(|button| button.bg(rgb(0x28282d)))
            .active(|button| button.opacity(0.72))
            .on_hover(cx.listener(Self::on_menu_hovered))
            .on_click(cx.listener(Self::on_menu_clicked))
            .child(
                svg()
                    .path("icons/menu.svg")
                    .size(px(20.0))
                    .text_color(menu_icon_color),
            );

        let bar = div()
            .id("snapbar")
            .flex()
            .items_center()
            .gap(px(12.0))
            .w_full()
            .h(px(60.0))
            .px(px(14.0))
            .rounded_full()
            .bg(rgb(0x0b0b0d))
            .shadow_lg()
            .child(target_button)
            .child(capture_button)
            .child(menu_button);

        let toggle = div()
            .flex()
            .items_center()
            .w(px(38.0))
            .h(px(22.0))
            .px(px(2.0))
            .rounded_full()
            .bg(if save_to_screenshots {
                rgb(0x2f8f5b)
            } else {
                rgb(0x343439)
            })
            .when(save_to_screenshots, |toggle| toggle.justify_end())
            .when(!save_to_screenshots, |toggle| toggle.justify_start())
            .child(div().size(px(18.0)).rounded_full().bg(rgb(0xf4f4f5)));

        let save_row = div()
            .id("save-toggle-row")
            .flex()
            .items_center()
            .justify_between()
            .w_full()
            .h(px(56.0))
            .px(px(12.0))
            .rounded(px(14.0))
            .cursor_pointer()
            .hover(|row| row.bg(rgb(0x1a1a1e)))
            .active(|row| row.opacity(0.76))
            .on_click(cx.listener(Self::on_save_toggle_clicked))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        svg()
                            .path("icons/folder.svg")
                            .size(px(18.0))
                            .text_color(rgb(0xc8c8cd)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(primary_text)
                                    .child("ファイルにも保存"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(secondary_text)
                                    .child("Windows スクリーンショット"),
                            ),
                    ),
            )
            .child(toggle);

        let refresh_row = menu_action(
            "refresh-row",
            "icons/refresh.svg",
            "会議・共有を再検出",
            primary_text,
        )
        .on_click(cx.listener(Self::on_refresh_clicked));

        let quit_row = menu_action(
            "quit-row",
            "icons/power.svg",
            "Snapbarを終了",
            rgb(0xf07178),
        )
        .on_click(|_, _, cx| cx.quit());

        let menu = div()
            .id("hover-menu")
            .flex()
            .flex_col()
            .gap(px(6.0))
            .w_full()
            .h(px(170.0))
            .p(px(10.0))
            .rounded(px(20.0))
            .bg(rgb(0x0d0d0f))
            .shadow_lg()
            .child(save_row)
            .child(refresh_row)
            .child(quit_row)
            .child(
                div()
                    .w_full()
                    .px(px(12.0))
                    .pt(px(2.0))
                    .text_xs()
                    .text_color(rgb(0x7e7e85))
                    .child(self.menu_summary()),
            );

        div()
            .id("root-hover-region")
            .flex()
            .flex_col()
            .gap(px(4.0))
            .size_full()
            .p(px(4.0))
            .bg(transparent_black())
            .on_hover(cx.listener(Self::on_root_hovered))
            .child(bar)
            .when(menu_open, |root| root.child(menu))
    }
}

fn menu_action(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    color: gpui::Rgba,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(10.0))
        .w_full()
        .h(px(38.0))
        .px(px(12.0))
        .rounded(px(12.0))
        .cursor_pointer()
        .text_sm()
        .text_color(color)
        .hover(|row| row.bg(rgb(0x1a1a1e)))
        .active(|row| row.opacity(0.76))
        .child(svg().path(icon).size(px(18.0)))
        .child(label)
}

pub fn run() {
    application().with_assets(Assets).run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(EXPANDED_HEIGHT)), cx);
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

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let shortened: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{shortened}…")
    } else {
        shortened
    }
}
