use crate::{
    assets::Assets,
    capture::{CaptureTarget, CropMode, capture_to_clipboard, discover_teams_targets},
};
use gpui::{
    App, Bounds, ClickEvent, Context, MouseButton, Window, WindowBackgroundAppearance,
    WindowBounds, WindowDecorations, WindowKind, WindowOptions, div, prelude::*, px, rgb, size,
    svg, transparent_black,
};
use gpui_platform::application;

const WINDOW_WIDTH: f32 = 304.0;
const COLLAPSED_HEIGHT: f32 = 64.0;
const EXPANDED_HEIGHT: f32 = 138.0;

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
    crop_mode: CropMode,
    position_locked: bool,
    menu_open: bool,
    capture_state: CaptureState,
    capture_count: u64,
    capture_generation: u64,
    last_error: Option<String>,
}

impl Snapbar {
    fn new() -> Self {
        let mut snapbar = Self {
            targets: Vec::new(),
            selected_target: 0,
            crop_mode: CropMode::FullWindow,
            position_locked: false,
            menu_open: false,
            capture_state: CaptureState::NoTarget,
            capture_count: 0,
            capture_generation: 0,
            last_error: None,
        };
        snapbar.refresh_targets(false);
        snapbar
    }

    fn current_target(&self) -> Option<&CaptureTarget> {
        self.targets.get(self.selected_target)
    }

    fn refresh_targets(&mut self, select_next: bool) {
        let previous_id = self.current_target().map(|target| target.id);

        match discover_teams_targets() {
            Ok(targets) if targets.is_empty() => {
                self.targets.clear();
                self.selected_target = 0;
                self.capture_state = CaptureState::NoTarget;
                self.last_error = Some("Teamsウィンドウを検出できません".to_string());
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
                self.capture_state = CaptureState::Idle;
                self.last_error = None;
            }
            Err(error) => {
                self.targets.clear();
                self.selected_target = 0;
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

    fn on_crop_clicked(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.crop_mode = self.crop_mode.toggled();
        if self.capture_state != CaptureState::Capturing {
            self.capture_state = if self.targets.is_empty() {
                CaptureState::NoTarget
            } else {
                CaptureState::Idle
            };
        }
        cx.notify();
    }

    fn on_lock_clicked(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.position_locked = !self.position_locked;
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

        if self.current_target().is_none() {
            self.refresh_targets(false);
        }
        let Some(target) = self.current_target().cloned() else {
            cx.notify();
            return;
        };

        self.capture_generation = self.capture_generation.wrapping_add(1);
        let generation = self.capture_generation;
        let crop_mode = self.crop_mode;
        self.capture_state = CaptureState::Capturing;
        self.last_error = None;
        cx.notify();

        let task = cx
            .background_executor()
            .spawn(async move { capture_to_clipboard(target.id, crop_mode) });

        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.capture_generation != generation {
                    return;
                }

                match result {
                    Ok(()) => {
                        this.capture_count = this.capture_count.saturating_add(1);
                        this.capture_state = CaptureState::Copied;
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

    fn target_summary(&self) -> String {
        if let Some(error) = &self.last_error {
            return format!("状態: {}", truncate(error, 34));
        }

        let Some(target) = self.current_target() else {
            return "対象: Teamsウィンドウなし".to_string();
        };
        let name = if target.title.trim().is_empty() {
            &target.app_name
        } else {
            &target.title
        };
        format!("対象: {}  ·  {}枚", truncate(name, 28), self.capture_count)
    }
}

impl Render for Snapbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_target = self.current_target().is_some();
        let position_locked = self.position_locked;
        let icon_color = rgb(0x8d8d93);
        let active_icon_color = rgb(0xf1f1f3);
        let status_color = match self.capture_state {
            CaptureState::Idle => rgb(0x85858b),
            CaptureState::Capturing => rgb(0xf2f2f4),
            CaptureState::Copied => rgb(0x65c987),
            CaptureState::NoTarget => rgb(0xd9943a),
            CaptureState::Error => rgb(0xe0525f),
        };
        let shutter_icon = match self.capture_state {
            CaptureState::Copied => "icons/check.svg",
            CaptureState::Error => "icons/alert.svg",
            _ => "icons/camera.svg",
        };

        let target_button = div()
            .id("target-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(34.0))
            .rounded_full()
            .cursor_pointer()
            .text_color(if has_target {
                active_icon_color
            } else {
                icon_color
            })
            .when(has_target, |button| button.bg(rgb(0x1b1b1e)))
            .hover(|button| button.bg(rgb(0x242428)))
            .active(|button| button.opacity(0.72))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(Self::on_target_clicked))
            .child(svg().path("icons/window.svg").size(px(18.0)));

        let crop_active = self.crop_mode == CropMode::TeamsContentPreset;
        let crop_button = div()
            .id("crop-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(34.0))
            .rounded_full()
            .cursor_pointer()
            .text_color(if crop_active {
                active_icon_color
            } else {
                icon_color
            })
            .when(crop_active, |button| button.bg(rgb(0x1b1b1e)))
            .hover(|button| button.bg(rgb(0x242428)))
            .active(|button| button.opacity(0.72))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(Self::on_crop_clicked))
            .child(svg().path("icons/crop.svg").size(px(18.0)));

        let lock_icon = if self.position_locked {
            "icons/lock-closed.svg"
        } else {
            "icons/lock-open.svg"
        };
        let lock_button = div()
            .id("lock-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(34.0))
            .rounded_full()
            .cursor_pointer()
            .text_color(if self.position_locked {
                active_icon_color
            } else {
                icon_color
            })
            .when(self.position_locked, |button| button.bg(rgb(0x1b1b1e)))
            .hover(|button| button.bg(rgb(0x242428)))
            .active(|button| button.opacity(0.72))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(Self::on_lock_clicked))
            .child(svg().path(lock_icon).size(px(18.0)));

        let more_button = div()
            .id("more-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(34.0))
            .rounded_full()
            .cursor_pointer()
            .text_color(if self.menu_open {
                active_icon_color
            } else {
                icon_color
            })
            .when(self.menu_open, |button| button.bg(rgb(0x1b1b1e)))
            .hover(|button| button.bg(rgb(0x242428)))
            .active(|button| button.opacity(0.72))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(Self::on_more_clicked))
            .child(svg().path("icons/more.svg").size(px(18.0)));

        let status_indicator = div()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(2.5))
            .w(px(38.0))
            .h(px(28.0))
            .child(div().w(px(2.0)).h(px(8.0)).rounded_full().bg(status_color))
            .child(div().w(px(2.0)).h(px(14.0)).rounded_full().bg(status_color))
            .child(div().w(px(2.0)).h(px(19.0)).rounded_full().bg(status_color))
            .child(div().w(px(2.0)).h(px(14.0)).rounded_full().bg(status_color))
            .child(div().w(px(2.0)).h(px(8.0)).rounded_full().bg(status_color));

        let capture_button = div()
            .id("capture-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(40.0))
            .rounded_full()
            .cursor_pointer()
            .bg(rgb(0xd83243))
            .text_color(rgb(0xffffff))
            .hover(|button| button.bg(rgb(0xe23d4f)))
            .active(|button| button.opacity(0.72))
            .when(self.capture_state == CaptureState::Capturing, |button| {
                button.opacity(0.72)
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(Self::on_capture_clicked))
            .child(svg().path(shutter_icon).size(px(19.0)));

        let bar = div()
            .id("snapbar")
            .flex()
            .items_center()
            .gap(px(3.0))
            .w_full()
            .h(px(56.0))
            .px(px(10.0))
            .rounded_full()
            .border_1()
            .border_color(rgb(0x27272b))
            .bg(rgb(0x09090a))
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, move |_, window, _| {
                if !position_locked {
                    window.start_window_move();
                }
            })
            .child(target_button)
            .child(crop_button)
            .child(lock_button)
            .child(more_button)
            .child(div().mx(px(4.0)).w(px(1.0)).h(px(24.0)).bg(rgb(0x343438)))
            .child(status_indicator)
            .child(capture_button);

        let menu = div()
            .id("more-menu")
            .flex()
            .flex_col()
            .gap(px(8.0))
            .w_full()
            .h(px(66.0))
            .px(px(12.0))
            .py(px(9.0))
            .rounded(px(14.0))
            .border_1()
            .border_color(rgb(0x27272b))
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
                            .w(px(126.0))
                            .h(px(26.0))
                            .rounded(px(8.0))
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
                            .w(px(126.0))
                            .h(px(26.0))
                            .rounded(px(8.0))
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
                is_movable: true,
                is_resizable: false,
                is_minimizable: false,
                window_background: WindowBackgroundAppearance::Transparent,
                window_decorations: Some(WindowDecorations::Client),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Snapbar::new()),
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
