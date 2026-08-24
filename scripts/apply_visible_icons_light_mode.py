from __future__ import annotations

import re
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if new in text:
        return text
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{label}: expected one match, found {count}")
    return text.replace(old, new, 1)


def patch_app() -> None:
    path = Path("src/app.rs")
    text = path.read_text(encoding="utf-8")
    text = replace_once(
        text,
        "const WINDOW_WIDTH: f32 = 252.0;\nconst COLLAPSED_HEIGHT: f32 = 66.0;\nconst EXPANDED_HEIGHT: f32 = 244.0;",
        "const WINDOW_WIDTH: f32 = 286.0;\nconst COLLAPSED_HEIGHT: f32 = 68.0;\nconst EXPANDED_HEIGHT: f32 = 246.0;",
        "window dimensions",
    )

    render = r'''impl Render for Snapbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_target = self.current_target().is_some();
        let menu_open = self.menu_open;
        let save_to_screenshots = self.settings.save_to_screenshots;
        let primary_text = rgb(0xf5f5f6);
        let secondary_text = rgb(0x96969d);
        let target_icon_color = match self.capture_state {
            CaptureState::Error => rgb(0xf07178),
            CaptureState::NoTarget => rgb(0xe0a24a),
            _ if has_target => rgb(0xd7d7dc),
            _ => secondary_text,
        };
        let capture_background = match self.capture_state {
            CaptureState::NoTarget => rgb(0x35353a),
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
            .cursor_pointer()
            .bg(capture_background)
            .shadow_sm()
            .hover(|button| button.opacity(0.91))
            .active(|button| button.opacity(0.68))
            .when(self.capture_state == CaptureState::Capturing, |button| {
                button.opacity(0.76)
            })
            .on_click(cx.listener(Self::on_capture_clicked))
            .child(
                svg()
                    .path("icons/camera.svg")
                    .size(px(20.0))
                    .text_color(rgb(0xffffff)),
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
            .gap(px(8.0))
            .size_full()
            .p(px(4.0))
            .bg(transparent_black())
            .on_hover(cx.listener(Self::on_root_hovered))
            .child(bar)
            .when(menu_open, |root| root.child(menu))
    }
}
'''

    pattern = re.compile(r"impl Render for Snapbar \{.*?\n\}\n\n(?=fn menu_action\()", re.S)
    text, count = pattern.subn(render + "\n", text, count=1)
    if count != 1:
        raise RuntimeError(f"render block: expected one match, found {count}")
    path.write_text(text, encoding="utf-8")


def patch_visual() -> None:
    path = Path("src/capture/content_detector/visual.rs")
    text = path.read_text(encoding="utf-8")

    text = replace_once(
        text,
        "        let background = estimate_analysis_background(&small);\n        let mut active = vec![false; (width * height) as usize];\n        let mut distance = vec![0u8; active.len()];",
        "        let background = estimate_analysis_background(&small);\n        let background_luminance = luminance(background);\n        let color_activity_threshold = if background_luminance >= 170.0 {\n            12.0\n        } else if background_luminance >= 125.0 {\n            18.0\n        } else {\n            COLOR_ACTIVITY_THRESHOLD\n        };\n        let edge_activity_threshold = if background_luminance >= 170.0 {\n            10.0\n        } else if background_luminance >= 125.0 {\n            15.0\n        } else {\n            EDGE_ACTIVITY_THRESHOLD\n        };\n        let mut active = vec![false; (width * height) as usize];\n        let mut distance = vec![0u8; active.len()];",
        "adaptive light-mode thresholds",
    )
    text = replace_once(
        text,
        "                active[pixel_index] = background_distance >= COLOR_ACTIVITY_THRESHOLD\n                    || (background_distance >= 8.0 && edge_strength >= EDGE_ACTIVITY_THRESHOLD);",
        "                active[pixel_index] = background_distance >= color_activity_threshold\n                    || (background_distance >= color_activity_threshold * 0.35\n                        && edge_strength >= edge_activity_threshold);",
        "adaptive activity mask",
    )

    choose_background = r'''fn choose_background_bucket(buckets: &HashMap<u16, ColorBucket>) -> [u8; 4] {
    if buckets.is_empty() {
        return [0, 0, 0, 255];
    }
    let total: u32 = buckets.values().map(|bucket| bucket.count).sum();
    let mut best: Option<(f32, &ColorBucket)> = None;

    for bucket in buckets.values() {
        let color = bucket.color();
        let chroma = color[0].max(color[1]).max(color[2])
            - color[0].min(color[1]).min(color[2]);
        let side_count = bucket.sides.count_ones();
        let frequency = bucket.count as f32 / total.max(1) as f32;
        let has_horizontal_pair = bucket.sides & 0b0011 == 0b0011;
        let has_vertical_pair = bucket.sides & 0b1100 == 0b1100;
        let opposite_pair_bonus = match (has_horizontal_pair, has_vertical_pair) {
            (true, true) => 0.48,
            (true, false) | (false, true) => 0.30,
            (false, false) => 0.0,
        };
        let neutrality = 1.0 - f32::from(chroma) / 255.0;
        let luminance_extremity = ((luminance(color) - 127.5).abs() / 127.5).min(1.0);
        let enough_evidence = frequency >= 0.012 || side_count >= 2;
        if !enough_evidence {
            continue;
        }

        // Teams may be dark or light. Prefer a stable, low-chroma color that occurs on
        // multiple perimeter sides instead of assuming the meeting background is dark.
        let score = frequency * 2.20
            + side_count as f32 * 0.22
            + opposite_pair_bonus
            + neutrality * 0.18
            + luminance_extremity * 0.08;
        if best.is_none_or(|(best_score, _)| score > best_score) {
            best = Some((score, bucket));
        }
    }

    best.map(|(_, bucket)| bucket.color()).unwrap_or_else(|| {
        buckets
            .values()
            .max_by_key(|bucket| bucket.count)
            .map_or([0, 0, 0, 255], ColorBucket::color)
    })
}
'''
    pattern = re.compile(
        r"fn choose_background_bucket\(buckets: &HashMap<u16, ColorBucket>\) -> \[u8; 4\] \{.*?\n\}\n\n(?=fn estimate_region_mode)",
        re.S,
    )
    text, count = pattern.subn(choose_background + "\n", text, count=1)
    if count != 1:
        raise RuntimeError(f"background selection: expected one match, found {count}")

    text = replace_once(
        text,
        "fn color_bucket_key(pixel: [u8; 4]) -> u16 {\n    (u16::from(pixel[0] >> 4) << 8) | (u16::from(pixel[1] >> 4) << 4) | u16::from(pixel[2] >> 4)\n}",
        "fn color_bucket_key(pixel: [u8; 4]) -> u16 {\n    (u16::from(pixel[0] >> 3) << 10)\n        | (u16::from(pixel[1] >> 3) << 5)\n        | u16::from(pixel[2] >> 3)\n}",
        "finer perimeter color buckets",
    )

    if "light_mode_side_participant_strip_is_removed" not in text:
        marker = "    #[test]\n    fn uniform_dark_content_is_not_shrunk_without_a_boundary() {"
        test = r'''    #[test]
    fn light_mode_side_participant_strip_is_removed_and_taskbar_is_kept() {
        let mut image = RgbaImage::from_pixel(1920, 900, Rgba([244, 245, 247, 255]));
        let shared = PixelRect::new(180, 0, 1240, 900);
        fill_rect(&mut image, shared, Rgba([252, 252, 253, 255]));
        fill_rect(
            &mut image,
            PixelRect::new(shared.x, 70, shared.width, 78),
            Rgba([234, 239, 248, 255]),
        );
        fill_rect(
            &mut image,
            PixelRect::new(shared.x, 148, shared.width, 702),
            Rgba([15, 92, 168, 255]),
        );
        fill_rect(
            &mut image,
            PixelRect::new(shared.x, 850, shared.width, 50),
            Rgba([238, 239, 242, 255]),
        );
        add_taskbar_icons(&mut image, PixelRect::new(shared.x, 850, shared.width, 50));

        for row in 0..3 {
            let tile = PixelRect::new(1450, 90 + row * 250, 430, 220);
            fill_rect(&mut image, tile, Rgba([249, 249, 250, 255]));
            fill_rect(
                &mut image,
                PixelRect::new(tile.x, tile.y, tile.width, 2),
                Rgba([218, 219, 224, 255]),
            );
            fill_rect(
                &mut image,
                PixelRect::new(tile.x + 165, tile.y + 54, 100, 100),
                Rgba([112 + row as u8 * 18, 145, 190, 255]),
            );
            fill_rect(
                &mut image,
                PixelRect::new(tile.x + 24, tile.bottom() - 32, 180, 10),
                Rgba([88, 89, 94, 255]),
            );
        }

        let detected = refine_uniform_margins(&image, PixelRect::new(0, 0, 1920, 900));
        assert!(detected.x >= 165 && detected.x <= 195, "{detected:?}");
        assert!(
            detected.right() >= 1400 && detected.right() <= 1440,
            "{detected:?}"
        );
        assert_eq!(detected.y, 0, "{detected:?}");
        assert_eq!(detected.bottom(), 900, "{detected:?}");
    }

'''
        if marker not in text:
            raise RuntimeError("light-mode test insertion marker not found")
        text = text.replace(marker, test + marker, 1)

    path.write_text(text, encoding="utf-8")


def patch_readme() -> None:
    path = Path("README.md")
    text = path.read_text(encoding="utf-8")
    text = text.replace(
        "現在のベータは、上下・左右の参加者表示を除外する共有面検出、余白を確保した外枠なしピルUI、任意のPNG保存に対応しています。共有されているWindowsデスクトップのタスクバーは共有コンテンツの一部として残します。",
        "現在のベータは、Teamsのライト／ダーク両テーマで上下・左右の参加者表示を除外する共有面検出、余白を確保した外枠なしピルUI、任意のPNG保存に対応しています。共有されているWindowsデスクトップのタスクバーは共有コンテンツの一部として残します。",
    )
    text = text.replace(
        "- 左側にはTeamsの接続状態を表示します。複数のTeams候補がある場合はクリックで切り替えます。\n- 中央の赤いカメラボタンで撮影します。\n- 右端の三本線メニューアイコンへマウスを乗せるかクリックすると設定メニューが開きます。クリックした場合は開いた状態で固定できます。",
        "- 左側には中立色のウィンドウアイコンとTeams対象名を表示します。複数のTeams候補がある場合はクリックで切り替えます。\n- 中央の赤いボタンは常に白いカメラアイコンを表示し、撮影成功後も色を別の意味へ切り替えません。\n- 右端には常時見える三本線メニューアイコンを表示します。マウスを乗せるかクリックすると設定メニューが開き、クリックした場合は開いた状態で固定できます。",
    )
    path.write_text(text, encoding="utf-8")


if __name__ == "__main__":
    patch_app()
    patch_visual()
    patch_readme()
