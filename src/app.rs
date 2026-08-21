use crate::{
    assets::Assets,
    capture::{
        CaptureEngine, CaptureTarget, discover_teams_targets, save_clipboard_image_to_screenshots,
        show_capture_flash,
    },
    overlay::TeamsWindowFollower,
    settings::AppSettings,
};
use gpui::{
    App, Bounds, ClickEvent, Context, Window, WindowBackgroundAppearance, WindowBounds,
    WindowDecorations, WindowKind, WindowOptions, div, prelude::*, px, rgb, size, svg,
    transparent_black,
};
use gpui_platform::application;

const WINDOW_WIDTH: f32 = 202.0;
const COLLAPSED_HEIGHT: f32 = 56.0;
const EXPANDED_HEIGHT: f32 = 204.0;

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
    settings: AppSettings,
    menu_open: bool,
    menu_pinned: bool,
    capture_state: CaptureState,
    capture_count: u64,
    capture_generation: u64,
    last_latency_ms: Option<u128>,
    last_saved_path: Option<String>,
    last_error: Option<String>,
}

impl Snapbar {
    fn new(window: &Window) -> Self {
        let mut snapbar = Self {
            targets: Vec::new(),
            selected_target: 0,
            capture_engine: None,
            follower: TeamsWindowFollower::start(window),
            settings: AppSettings::load(),
            menu_open: false,
            menu_pinned: false,
            capture_state: CaptureState::NoTarget,
            capture_count: 0,
            capture_generation: 0,
            last_latency_ms: None,
            last_saved_path: None,
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

    fn set_menu_open(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu_open == open {
            return;
        }
        self.menu_open = open;
        let height = if open {
            EXPANDED_HEIGHT
        } else {
            COLLAPSED_HEIGHT
        };
        window.resize(size(px(WINDOW_WIDTH), px(height)));
        cx.notify();
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
        self.menu_pinned = !self.menu_pinned;
        self.set_menu_open(self.menu_pinned, window, cx);
    }

    fn on_more_hovered(&mut self, hovered: &bool, window: &mut Window, cx: &mut Context<Self>) {
        if *hovered {
            self.set_menu_open(true, window, cx);
        }
    }

    fn on_root_hovered(&mut self, hovered: &bool, window: &mut Window, cx: &mut Context<Self>) {
        if !*hovered && !self.menu_pinned {
            self.set_menu_open(false, window, cx);
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

        if self.capture_engine.is_none() {
            self.refresh_targets(false);
        }
        let Some(engine) = self.capture_engine.clone() else {
            cx.notify();
            return;
        };

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
            return "未接続".to_string();
        }
        if self.targets.len() <= 1 {
            "Teams".to_string()
        } else {
            format!("Teams {}/{}", self.selected_target + 1, self.targets.len())
        }
    }

    fn status_label(&self) -> &'static str {
        match self.capture_state {
            CaptureState::Idle => "準備完了",
            CaptureState::Capturing => "撮影中",
            CaptureState::Copied if self.settings.save_to_screenshots => "コピー・保存済み",
            CaptureState::Copied => "コピー済み",
            CaptureState::NoTarget => "対象なし",
            CaptureState::Error => "要確認",
        }
    }

    fn menu_summary(&self) -> String {
        if let Some(error) = &self.last_error {
            return truncate(error, 38);
        }
        if let Some(path) = &self.last_saved_path {
            return format!("保存: {}", truncate(path, 31));
        }
        let latency = self
            .last_latency_ms
            .map(|value| format!(" · {value}ms"))
            .unwrap_or_default();
        format!("{}枚{latency}", self.capture_count)
    }
}

impl Render for Snapbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_target = self.current_target().is_some();
        let menu_open = self.menu_open;
        let save_to_screenshots = self.settings.save_to_screenshots;
        let primary_text = rgb(0xf4f4f5);
        let secondary_text = rgb(0x929299);
        let target_dot = if has_target {
            rgb(0x62d38b)
        } else {
            rgb(0xe0a24a)
        };
        let (capture_icon, capture_background) = match self.capture_state {
            CaptureState::Idle => ("icons/camera.svg", rgb(0xe5484d)),
            CaptureState::Capturing => ("icons/camera.svg", rgb(0xa92f3a)),
            CaptureState::Copied => ("icons/check.svg", rgb(0x2d8654)),
            CaptureState::NoTarget => ("icons/camera.svg", rgb(0x34343a)),
            CaptureState::Error => ("icons/alert.svg", rgb(0xbd3544)),
        };

        let target_button = div()
            .id("target-button")
            .flex()
            .items_center()
            .gap(px(7.0))
            .w(px(92.0))
            .h(px(38.0))
            .px(px(8.0))
            .rounded(px(12.0))
            .cursor_pointer()
            .hover(|button| button.bg(rgb(0x1a1a1d)))
            .active(|button| button.opacity(0.72))
            .on_click(cx.listener(Self::on_target_clicked))
            .child(div().size(px(7.0)).rounded_full().bg(target_dot))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(
                        div()
                            .text_sm()
                            .text_color(primary_text)
                            .child(self.target_label()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(secondary_text)
                            .child(self.status_label()),
                    ),
            );

        let capture_button = div()
            .id("capture-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(40.0))
            .rounded_full()
            .cursor_pointer()
            .bg(capture_background)
            .text_color(rgb(0xffffff))
            .shadow_sm()
            .hover(|button| button.opacity(0.90))
            .active(|button| button.opacity(0.68))
            .when(self.capture_state == CaptureState::Capturing, |button| {
                button.opacity(0.74)
            })
            .on_click(cx.listener(Self::on_capture_clicked))
            .child(svg().path(capture_icon).size(px(18.0)));

        let more_button = div()
            .id("more-button")
            .flex()
            .items_center()
            .justify_center()
            .size(px(32.0))
            .rounded_full()
            .cursor_pointer()
            .text_color(if menu_open {
                primary_text
            } else {
                secondary_text
            })
            .when(menu_open, |button| button.bg(rgb(0x1a1a1d)))
            .hover(|button| button.bg(rgb(0x202024)).text_color(primary_text))
            .active(|button| button.opacity(0.72))
            .on_hover(cx.listener(Self::on_more_hovered))
            .on_click(cx.listener(Self::on_more_clicked))
            .child(svg().path("icons/more.svg").size(px(17.0)));

        let bar = div()
            .id("snapbar")
            .flex()
            .items_center()
            .justify_between()
            .w_full()
            .h(px(50.0))
            .px(px(7.0))
            .rounded_full()
            .bg(rgb(0x0b0b0d))
            .shadow_lg()
            .child(target_button)
            .child(capture_button)
            .child(more_button);

        let toggle = div()
            .flex()
            .items_center()
            .w(px(34.0))
            .h(px(20.0))
            .px(px(2.0))
            .rounded_full()
            .bg(if save_to_screenshots {
                rgb(0x2f8f5b)
            } else {
                rgb(0x333338)
            })
            .when(save_to_screenshots, |toggle| toggle.justify_end())
            .when(!save_to_screenshots, |toggle| toggle.justify_start())
            .child(div().size(px(16.0)).rounded_full().bg(rgb(0xf4f4f5)));

        let save_row = div()
            .id("save-toggle-row")
            .flex()
            .items_center()
            .justify_between()
            .w_full()
            .h(px(52.0))
            .px(px(10.0))
            .rounded(px(12.0))
            .cursor_pointer()
            .hover(|row| row.bg(rgb(0x1a1a1d)))
            .active(|row| row.opacity(0.76))
            .on_click(cx.listener(Self::on_save_toggle_clicked))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .child(
                        svg()
                            .path("icons/folder.svg")
                            .size(px(17.0))
                            .text_color(rgb(0xc5c5ca)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
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
            "対象を再検出",
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
            .gap(px(4.0))
            .w_full()
            .h(px(142.0))
            .p(px(6.0))
            .rounded(px(18.0))
            .bg(rgb(0x0d0d0f))
            .shadow_lg()
            .child(save_row)
            .child(refresh_row)
            .child(quit_row)
            .child(
                div()
                    .w_full()
                    .px(px(10.0))
                    .pt(px(2.0))
                    .text_xs()
                    .text_color(rgb(0x77777e))
                    .child(self.menu_summary()),
            );

        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .size_full()
            .p(px(3.0))
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
) -> gpui::Div {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(9.0))
        .w_full()
        .h(px(32.0))
        .px(px(10.0))
        .rounded(px(10.0))
        .cursor_pointer()
        .text_sm()
        .text_color(color)
        .hover(|row| row.bg(rgb(0x1a1a1d)))
        .active(|row| row.opacity(0.76))
        .child(svg().path(icon).size(px(17.0)))
        .child(label)
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
                focus: false,
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
