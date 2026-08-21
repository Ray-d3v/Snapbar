use crate::{
    assets::Assets,
    capture::{CaptureEngine, CaptureTarget, discover_teams_targets, show_capture_flash},
    overlay::TeamsWindowFollower,
};
use gpui::{
    App, Bounds, ClickEvent, Context, Window, WindowBackgroundAppearance, WindowBounds,
    WindowDecorations, WindowKind, WindowOptions, div, prelude::*, px, rgb, size, svg,
    transparent_black,
};
use gpui_platform::application;

const WINDOW_WIDTH: f32 = 248.0;
const COLLAPSED_HEIGHT: f32 = 58.0;
const EXPANDED_HEIGHT: f32 = 132.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureState {
    Idle,
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
    menu_open: bool,
    capture_state: CaptureState,
    capture_count: u64,
    capture_generation: u64,
    last_latency_ms: Option<u128>,
    last_error: Option<String>,
}

impl Snapbar {
    fn new(window: &Window) -> Self {
        let mut snapbar = Self {
            targets: Vec::new(),
            selected_target: 0,
            capture_engine: None,
            follower: TeamsWindowFollower::start(window),
            menu_open: false,
            capture_state: CaptureState::NoTarget,
            capture_count: 0,
            capture_generation: 0,
            last_latency_ms: None,
            last_error: None,
        };
        snapbar.refresh_targets(false);
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

    fn refresh_targets(&mut self, select_next: bool) {
        let previous_id = self.current_target().map(|target| target.id);
        self.capture_generation = self.capture_generation.wrapping_add(1);
        self.capture_engine = None;

        match discover_teams_targets() {
            Ok(targets) if targets.is_empty() => {
                self.targets.clear();
                self.selected_target = 0;
                self.capture_state = CaptureState::NoTarget;
                self.last_error = Some("Teams会議画面を検出できません".to_string());
                self.sync_follower();
            }
            Ok(targets) => {
                let previous_index = previous_id
                    .and_then(|id| targets.iter().position(|target| target.id == id))
                    .unwrap_or(0);
                self.selected_target = if select_next && targets.len() > 1 {
                    (previous_index + 1) % targets.len()
                } else {
                    previous_index
                };
                self.targets = targets;
                self.restart_capture_engine();
            }
            Err(error) => {
                self.targets.clear();
                self.selected_target = 0;
                self.capture_state = CaptureState::Error;
                self.last_error = Some(error.to_string());
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
            self.last_error = Some("Teams会議画面を検出できません".to_string());
            return;
        };

        match CaptureEngine::start(target_id) {
            Ok(engine) => {
                self.capture_engine = Some(engine);
                self.capture_state = CaptureState::Idle;
                self.last_error = None;
            }
            Err(error) => {
                self.capture_state = CaptureState::Error;
                self.last_error = Some(error.to_string());
            }
        }
    }

    fn on_target_clicked(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.refresh_targets(true);
        cx.notify();
    }

    fn on_refresh_clicked(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.refresh_targets(false);
        cx.notify();
    }

    fn on_more_clicked(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.menu_open = !self.menu_open;
        let height = if self.menu_open {
            EXPANDED_HEIGHT
        } else {
            COLLAPSED_HEIGHT
        };
        window.resize(size(px(WINDOW_WIDTH), px(height)));
        cx.notify();
    }

    fn on_capture_clicked(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.capture_state == CaptureState::Capturing {
            return;
        }

        if self.capture_engine.is_none() {
            self.refresh_targets(false);
            cx.notify();
            return;
        }
        let Some(engine) = self.capture_engine.clone() else {
            cx.notify();
            return;
        };

        self.capture_generation = self.capture_generation.wrapping_add(1);
        let generation = self.capture_generation;
        self.capture_state = CaptureState::Capturing;
        self.last_error = None;
        cx.notify();

        let task = cx
            .background_executor()
            .spawn(async move { engine.copy_latest_to_clipboard() });

        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.capture_generation != generation {
                    return;
                }

                match result {
                    Ok(receipt) => {
                        show_capture_flash(receipt.screen_rect);
                        let _ = receipt.frame_age;
                        this.capture_count = this.capture_count.saturating_add(1);
                        this.capture_state = CaptureState::Copied;
                        this.last_latency_ms = Some(receipt.latency.as_millis());
                        this.last_error = None;
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
            return "未検出".to_string();
        }
        if self.targets.len() <= 1 {
            "Teams".to_string()
        } else {
            format!("Teams {}/{}", self.selected_target + 1, self.targets.len())
        }
    }

    fn target_summary(&self) -> String {
        if let Some(error) = &self.last_error {
            return format!("状態: {}", truncate(error, 34));
        }

        let Some(target) = self.current_target() else {
            return "対象: Teams会議画面なし".to_string();
        };
        let name = if target.title.trim().is_empty() {
            &target.app_name
        } else {
            &target.title
        };
        let latency = self
            .last_latency_ms
            .map(|value| format!(" · {value}ms"))
            .unwrap_or_default();
        format!(
            "追従中: {} · {}枚{}",
            truncate(name, 24),
            self.capture_count,
            latency
        )
    }
}

impl Render for Snapbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_target = self.current_target().is_some();
        let icon_color = rgb(0xf1f1f3);
        let muted_icon_color = rgb(0x9a9aa0);
        let target_dot_color = if has_target {
            rgb(0x55c77a)
        } else {
            rgb(0xd9943a)
        };
        let (capture_label, capture_icon, capture_background) = match self.capture_state {
            CaptureState::Idle => ("スクショ", "icons/camera.svg", rgb(0xd83243)),
            CaptureState::Capturing => ("撮影中", "icons/camera.svg", rgb(0xa92535)),
            CaptureState::Copied => ("コピー済み", "icons/check.svg", rgb(0x267a48)),
            CaptureState::NoTarget => ("再検出", "icons/camera.svg", rgb(0x3a3a40)),
            CaptureState::Error => ("再試行", "icons/alert.svg", rgb(0xb52c3d)),
        };

        let target_button = div()
            .id("target-button")
            .flex()
            .items_center()
            .justify_center()
            .gap(px(7.0))
            .w(px(88.0))
            .h(px(36.0))
            .rounded(px(12.0))
            .cursor_pointer()
            .bg(rgb(0x1a1a1e))
            .text_sm()
            .text_color(if has_target {
                icon_color
            } else {
                muted_icon_color
            })
            .hover(|button| button.bg(rgb(0x242428)))
            .active(|button| button.opacity(0.72))
            .on_click(cx.listener(Self::on_target_clicked))
            .child(div().size(px(7.0)).rounded_full().bg(target_dot_color))
            .child(self.target_label());

        let capture_button = div()
            .id("capture-button")
            .flex()
            .items_center()
            .justify_center()
            .gap(px(7.0))
            .w(px(106.0))
            .h(px(40.0))
            .rounded(px(13.0))
            .cursor_pointer()
            .bg(capture_background)
            .text_sm()
            .text_color(rgb(0xffffff))
            .hover(|button| button.opacity(0.90))
            .active(|button| button.opacity(0.72))
            .when(self.capture_state == CaptureState::Capturing, |button| {
                button.opacity(0.76)
            })
            .on_click(cx.listener(Self::on_capture_clicked))
            .child(svg().path(capture_icon).size(px(18.0)))
            .child(capture_label);

        let more_button = div()
            .id("more-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(34.0))
            .rounded(px(11.0))
            .cursor_pointer()
            .text_color(if self.menu_open {
                icon_color
            } else {
                muted_icon_color
            })
            .when(self.menu_open, |button| button.bg(rgb(0x1a1a1e)))
            .hover(|button| button.bg(rgb(0x242428)))
            .active(|button| button.opacity(0.72))
            .on_click(cx.listener(Self::on_more_clicked))
            .child(svg().path("icons/more.svg").size(px(18.0)));

        let bar = div()
            .id("snapbar")
            .flex()
            .items_center()
            .justify_between()
            .w_full()
            .h(px(50.0))
            .px(px(7.0))
            .rounded(px(17.0))
            .border_1()
            .border_color(rgb(0x2d2d32))
            .bg(rgb(0x0a0a0c))
            .shadow_lg()
            .child(target_button)
            .child(capture_button)
            .child(more_button);

        let menu = div()
            .id("more-menu")
            .flex()
            .flex_col()
            .gap(px(8.0))
            .w_full()
            .h(px(68.0))
            .px(px(10.0))
            .py(px(9.0))
            .rounded(px(14.0))
            .border_1()
            .border_color(rgb(0x2d2d32))
            .bg(rgb(0x0d0d0f))
            .shadow_lg()
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xa5a5aa))
                    .child(self.target_summary()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .id("refresh-button")
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(106.0))
                            .h(px(28.0))
                            .rounded(px(9.0))
                            .cursor_pointer()
                            .bg(rgb(0x1b1b1e))
                            .text_sm()
                            .text_color(rgb(0xe8e8eb))
                            .hover(|button| button.bg(rgb(0x26262a)))
                            .on_click(cx.listener(Self::on_refresh_clicked))
                            .child("再検出"),
                    )
                    .child(
                        div()
                            .id("quit-button")
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(106.0))
                            .h(px(28.0))
                            .rounded(px(9.0))
                            .cursor_pointer()
                            .bg(rgb(0x1b1b1e))
                            .text_sm()
                            .text_color(rgb(0xf06a76))
                            .hover(|button| button.bg(rgb(0x26262a)))
                            .on_click(|_, _, cx| cx.quit())
                            .child("終了"),
                    ),
            );

        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .size_full()
            .p(px(4.0))
            .bg(transparent_black())
            .child(bar)
            .when(self.menu_open, |root| root.child(menu))
    }
}

pub fn run() {
    application().with_assets(Assets).run(|cx: &mut App| {
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(COLLAPSED_HEIGHT)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                kind: WindowKind::PopUp,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                window_background: WindowBackgroundAppearance::Transparent,
                window_decorations: Some(WindowDecorations::Client),
                ..Default::default()
            },
            |window, cx| cx.new(|_| Snapbar::new(window)),
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
