use image::RgbaImage;

use super::{ContentCandidate, PixelRect};

const DARK_TEAMS_BACKGROUND: u8 = 0x47;
const LIGHT_TEAMS_BACKGROUND: u8 = 0xb2;
const BORDER_PALETTES: [BorderPalette; 2] = [
    BorderPalette {
        outer: 0x72,
        inner: 0x60,
    },
    BorderPalette {
        outer: 0xa8,
        inner: 0x9b,
    },
];

#[derive(Clone, Copy)]
struct BorderPalette {
    outer: u8,
    inner: u8,
}

#[derive(Clone, Copy)]
enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy)]
enum SeparatorSide {
    Left,
    Right,
    Top,
}

#[derive(Clone, Copy)]
struct Separator {
    start: u32,
    end: u32,
    score: f32,
    side: SeparatorSide,
}

#[derive(Clone, Copy)]
struct ThemeBackground {
    color: [u8; 4],
    confidence: f32,
}

#[derive(Clone, Copy)]
struct RefinedRect {
    rect: PixelRect,
    confidence: f32,
}

pub(super) fn detect_teams_candidate(
    image: &RgbaImage,
    rect: PixelRect,
) -> Option<ContentCandidate> {
    let refined = refine_with_evidence(image, rect)?;
    Some(ContentCandidate::new(refined.rect, refined.confidence))
}

pub(super) fn refine_teams_rect(image: &RgbaImage, rect: PixelRect) -> PixelRect {
    refine_with_evidence(image, rect).map_or(rect, |refined| refined.rect)
}

fn refine_with_evidence(image: &RgbaImage, rect: PixelRect) -> Option<RefinedRect> {
    if rect.width < 160
        || rect.height < 90
        || rect.right() > image.width()
        || rect.bottom() > image.height()
    {
        return None;
    }

    let (stage, separator_confidence) = crop_by_triple_separator(image, rect);
    let image_area = u64::from(image.width()) * u64::from(image.height());
    let is_broad_stage = stage.area().saturating_mul(100) >= image_area.saturating_mul(68);
    let (surface, background_confidence) = if separator_confidence >= 0.62 || is_broad_stage {
        trim_known_teams_background(image, stage)
    } else {
        (stage, 0.0)
    };

    if surface == rect {
        return None;
    }
    let retained_area = surface.area() as f32 / rect.area().max(1) as f32;
    if retained_area < 0.18
        || surface.width * 100 < rect.width * 34
        || surface.height * 100 < rect.height * 34
    {
        return None;
    }

    let confidence =
        (0.72 + separator_confidence * 0.17 + background_confidence * 0.11).clamp(0.0, 0.99);
    Some(RefinedRect {
        rect: surface,
        confidence,
    })
}

fn crop_by_triple_separator(image: &RgbaImage, rect: PixelRect) -> (PixelRect, f32) {
    let vertical = detect_separator(image, rect, Axis::Vertical);
    let horizontal = detect_separator(image, rect, Axis::Horizontal);

    let mut left = rect.x;
    let mut right = rect.right();
    let mut top = rect.y;
    let mut confidence = 0.0_f32;

    if let Some(separator) = vertical {
        match separator.side {
            SeparatorSide::Left => left = separator.end,
            SeparatorSide::Right => right = separator.start,
            SeparatorSide::Top => {}
        }
        confidence = confidence.max(separator.score);
    }
    if let Some(separator) = horizontal {
        if matches!(separator.side, SeparatorSide::Top) {
            top = separator.end;
            confidence = confidence.max(separator.score);
        }
    }

    if right <= left || rect.bottom() <= top {
        return (rect, 0.0);
    }
    let candidate = PixelRect::new(left, top, right - left, rect.bottom() - top);
    let enough_width = candidate.width * 100 >= rect.width * 42;
    let enough_height = candidate.height * 100 >= rect.height * 42;
    let enough_area = candidate.area() * 100 >= rect.area() * 28;
    if enough_width && enough_height && enough_area {
        (candidate, confidence)
    } else {
        (rect, 0.0)
    }
}

fn detect_separator(image: &RgbaImage, rect: PixelRect, axis: Axis) -> Option<Separator> {
    let short_length = match axis {
        Axis::Horizontal => rect.height,
        Axis::Vertical => rect.width,
    };
    let max_band_width = (short_length / 360).clamp(1, 6);
    let search_start = match axis {
        Axis::Horizontal => rect.y.saturating_add((rect.height / 40).max(2)),
        Axis::Vertical => rect.x.saturating_add((rect.width / 40).max(2)),
    };
    let search_end = match axis {
        Axis::Horizontal => rect
            .y
            .saturating_add(rect.height * 48 / 100)
            .min(rect.bottom().saturating_sub(3)),
        Axis::Vertical => rect.right().saturating_sub(3),
    };
    if search_end <= search_start {
        return None;
    }

    let profile_length = search_end.saturating_sub(search_start) as usize;
    let mut best: Option<Separator> = None;

    for palette in BORDER_PALETTES {
        let mut outer_profile = vec![0.0_f32; profile_length];
        let mut inner_profile = vec![0.0_f32; profile_length];
        for offset in 0..profile_length {
            let position = search_start + offset as u32;
            outer_profile[offset] =
                band_match_ratio(image, rect, axis, position, palette.outer, 10);
            inner_profile[offset] =
                band_match_ratio(image, rect, axis, position, palette.inner, 10);
        }

        for start_offset in 0..profile_length {
            for outer_width in 1..=max_band_width {
                for inner_width in 1..=max_band_width {
                    for trailing_width in 1..=max_band_width {
                        let total_width = outer_width + inner_width + trailing_width;
                        let end_offset = start_offset.saturating_add(total_width as usize);
                        if end_offset > profile_length {
                            continue;
                        }

                        let first_outer =
                            average_profile(&outer_profile, start_offset, outer_width as usize);
                        let inner = average_profile(
                            &inner_profile,
                            start_offset + outer_width as usize,
                            inner_width as usize,
                        );
                        let second_outer = average_profile(
                            &outer_profile,
                            start_offset + outer_width as usize + inner_width as usize,
                            trailing_width as usize,
                        );
                        if first_outer < 0.48 || inner < 0.44 || second_outer < 0.34 {
                            continue;
                        }

                        let start = search_start + start_offset as u32;
                        let end = start.saturating_add(total_width);
                        let (start, end) = expand_separator_bounds(
                            image,
                            rect,
                            axis,
                            start,
                            end,
                            palette,
                            max_band_width.saturating_mul(3),
                        );
                        let Some((side, position_score)) = separator_side(rect, axis, start, end)
                        else {
                            continue;
                        };
                        let outside_score =
                            outside_background_ratio(image, rect, axis, start, end, side);
                        let continuity = (first_outer + inner + second_outer) / 3.0;
                        let shape = first_outer.min(second_outer) * 0.55 + inner * 0.45;
                        let score = (continuity * 0.54
                            + shape * 0.18
                            + position_score * 0.16
                            + outside_score * 0.12)
                            .clamp(0.0, 1.0);
                        if score < 0.62 {
                            continue;
                        }

                        let candidate = Separator {
                            start,
                            end,
                            score,
                            side,
                        };
                        if best.is_none_or(|current| candidate.score > current.score) {
                            best = Some(candidate);
                        }
                    }
                }
            }
        }
    }

    best
}

fn expand_separator_bounds(
    image: &RgbaImage,
    rect: PixelRect,
    axis: Axis,
    start: u32,
    end: u32,
    palette: BorderPalette,
    max_expansion: u32,
) -> (u32, u32) {
    let minimum = match axis {
        Axis::Horizontal => rect.y,
        Axis::Vertical => rect.x,
    };
    let maximum = match axis {
        Axis::Horizontal => rect.bottom(),
        Axis::Vertical => rect.right(),
    };

    let mut expanded_start = start;
    for _ in 0..max_expansion {
        if expanded_start <= minimum {
            break;
        }
        let position = expanded_start - 1;
        let match_ratio = band_match_ratio(image, rect, axis, position, palette.outer, 10).max(
            band_match_ratio(image, rect, axis, position, palette.inner, 10),
        );
        if match_ratio < 0.42 {
            break;
        }
        expanded_start = position;
    }

    let mut expanded_end = end;
    for _ in 0..max_expansion {
        if expanded_end >= maximum {
            break;
        }
        let match_ratio = band_match_ratio(image, rect, axis, expanded_end, palette.outer, 10).max(
            band_match_ratio(image, rect, axis, expanded_end, palette.inner, 10),
        );
        if match_ratio < 0.42 {
            break;
        }
        expanded_end += 1;
    }

    (expanded_start, expanded_end)
}

fn band_match_ratio(
    image: &RgbaImage,
    rect: PixelRect,
    axis: Axis,
    position: u32,
    target_gray: u8,
    tolerance: u8,
) -> f32 {
    let (start, end) = match axis {
        Axis::Horizontal => {
            let margin = (rect.width / 20).max(2);
            (
                rect.x.saturating_add(margin),
                rect.right().saturating_sub(margin),
            )
        }
        Axis::Vertical => {
            let margin = (rect.height / 20).max(2);
            (
                rect.y.saturating_add(margin),
                rect.bottom().saturating_sub(margin),
            )
        }
    };
    if end <= start {
        return 0.0;
    }

    let step = ((end - start) / 192).max(1);
    let mut samples = 0u32;
    let mut matches = 0u32;
    let mut cursor = start;
    while cursor < end {
        let pixel = match axis {
            Axis::Horizontal => image.get_pixel(cursor, position).0,
            Axis::Vertical => image.get_pixel(position, cursor).0,
        };
        samples += 1;
        if is_neutral_gray(pixel) && gray_value(pixel).abs_diff(target_gray) <= tolerance {
            matches += 1;
        }
        cursor = cursor.saturating_add(step);
    }

    if samples == 0 {
        0.0
    } else {
        matches as f32 / samples as f32
    }
}

fn average_profile(profile: &[f32], start: usize, length: usize) -> f32 {
    if length == 0 || start >= profile.len() || start + length > profile.len() {
        return 0.0;
    }
    profile[start..start + length].iter().sum::<f32>() / length as f32
}

fn separator_side(
    rect: PixelRect,
    axis: Axis,
    start: u32,
    end: u32,
) -> Option<(SeparatorSide, f32)> {
    let center = (start as f32 + end as f32) / 2.0;
    match axis {
        Axis::Horizontal => {
            let ratio = (center - rect.y as f32) / rect.height as f32;
            if (0.04..=0.48).contains(&ratio) {
                Some((SeparatorSide::Top, 1.0 - (ratio - 0.24).abs().min(0.24)))
            } else {
                None
            }
        }
        Axis::Vertical => {
            let ratio = (center - rect.x as f32) / rect.width as f32;
            if (0.04..=0.46).contains(&ratio) {
                Some((SeparatorSide::Left, 0.72 + (0.46 - ratio) * 0.25))
            } else if (0.54..=0.97).contains(&ratio) {
                Some((SeparatorSide::Right, 0.72 + (ratio - 0.54) * 0.25))
            } else {
                None
            }
        }
    }
}

fn outside_background_ratio(
    image: &RgbaImage,
    rect: PixelRect,
    axis: Axis,
    start: u32,
    end: u32,
    side: SeparatorSide,
) -> f32 {
    let outside = match (axis, side) {
        (Axis::Horizontal, SeparatorSide::Top) => {
            PixelRect::new(rect.x, rect.y, rect.width, start.saturating_sub(rect.y))
        }
        (Axis::Vertical, SeparatorSide::Left) => {
            PixelRect::new(rect.x, rect.y, start.saturating_sub(rect.x), rect.height)
        }
        (Axis::Vertical, SeparatorSide::Right) => {
            PixelRect::new(end, rect.y, rect.right().saturating_sub(end), rect.height)
        }
        _ => return 0.0,
    };
    if outside.width == 0 || outside.height == 0 {
        return 0.0;
    }

    let step_x = (outside.width / 96).max(1);
    let step_y = (outside.height / 96).max(1);
    let mut samples = 0u32;
    let mut matches = 0u32;
    let mut y = outside.y;
    while y < outside.bottom().min(image.height()) {
        let mut x = outside.x;
        while x < outside.right().min(image.width()) {
            let pixel = image.get_pixel(x, y).0;
            samples += 1;
            if is_neutral_gray(pixel) && is_near_known_background(gray_value(pixel), 24) {
                matches += 1;
            }
            x = x.saturating_add(step_x);
        }
        y = y.saturating_add(step_y);
    }

    if samples == 0 {
        0.0
    } else {
        ((matches as f32 / samples as f32) / 0.28).min(1.0)
    }
}

fn trim_known_teams_background(image: &RgbaImage, rect: PixelRect) -> (PixelRect, f32) {
    let Some(background) = estimate_theme_background(image, rect) else {
        return (rect, 0.0);
    };
    if background.confidence < 0.52 {
        return (rect, 0.0);
    }

    let max_vertical = rect.height * 46 / 100;
    let max_horizontal = rect.width * 46 / 100;
    let top_trim = background_row_run(image, rect, background.color, max_vertical, true);
    let bottom_trim = background_row_run(image, rect, background.color, max_vertical, false);
    let vertical = PixelRect::new(
        rect.x,
        rect.y.saturating_add(top_trim),
        rect.width,
        rect.height
            .saturating_sub(top_trim.saturating_add(bottom_trim)),
    );
    if vertical.height * 100 < rect.height * 34 {
        return (rect, 0.0);
    }

    let left_trim = background_column_run(image, vertical, background.color, max_horizontal, true);
    let right_trim =
        background_column_run(image, vertical, background.color, max_horizontal, false);
    let candidate = PixelRect::new(
        vertical.x.saturating_add(left_trim),
        vertical.y,
        vertical
            .width
            .saturating_sub(left_trim.saturating_add(right_trim)),
        vertical.height,
    );
    if candidate.width * 100 < rect.width * 34
        || candidate.height * 100 < rect.height * 34
        || candidate.area() * 100 < rect.area() * 18
    {
        return (rect, 0.0);
    }

    let trimmed_pixels = top_trim
        .saturating_add(bottom_trim)
        .saturating_add(left_trim)
        .saturating_add(right_trim);
    if trimmed_pixels < 2 {
        (rect, 0.0)
    } else {
        let trim_ratio = 1.0 - candidate.area() as f32 / rect.area().max(1) as f32;
        (
            candidate,
            (background.confidence * 0.72 + trim_ratio.min(0.5) * 0.56).min(1.0),
        )
    }
}

fn estimate_theme_background(image: &RgbaImage, rect: PixelRect) -> Option<ThemeBackground> {
    let band = (rect.width.min(rect.height) / 36).clamp(1, 8);
    let step = (rect.width.max(rect.height) / 768).max(1);
    let mut dark_samples = Vec::new();
    let mut light_samples = Vec::new();
    let mut total = 0u32;

    for y in rect.y..rect.y.saturating_add(band).min(rect.bottom()) {
        sample_theme_row(
            image,
            rect,
            y,
            step,
            &mut dark_samples,
            &mut light_samples,
            &mut total,
        );
    }
    for y in rect.bottom().saturating_sub(band)..rect.bottom() {
        sample_theme_row(
            image,
            rect,
            y,
            step,
            &mut dark_samples,
            &mut light_samples,
            &mut total,
        );
    }
    for x in rect.x..rect.x.saturating_add(band).min(rect.right()) {
        sample_theme_column(
            image,
            rect,
            x,
            step,
            &mut dark_samples,
            &mut light_samples,
            &mut total,
        );
    }
    for x in rect.right().saturating_sub(band)..rect.right() {
        sample_theme_column(
            image,
            rect,
            x,
            step,
            &mut dark_samples,
            &mut light_samples,
            &mut total,
        );
    }

    let (mut samples, anchor) = if dark_samples.len() >= light_samples.len() {
        (dark_samples, DARK_TEAMS_BACKGROUND)
    } else {
        (light_samples, LIGHT_TEAMS_BACKGROUND)
    };
    if samples.len() < 8 || total == 0 {
        return None;
    }
    samples.sort_unstable();
    let observed = samples[samples.len() / 2];
    let frequency = samples.len() as f32 / total as f32;
    let anchor_score = (1.0 - observed.abs_diff(anchor) as f32 / 30.0).clamp(0.0, 1.0);
    let confidence = (frequency / 0.28).min(1.0) * 0.64 + anchor_score * 0.36;
    Some(ThemeBackground {
        color: [observed, observed, observed, 255],
        confidence: confidence.min(1.0),
    })
}

fn sample_theme_row(
    image: &RgbaImage,
    rect: PixelRect,
    y: u32,
    step: u32,
    dark_samples: &mut Vec<u8>,
    light_samples: &mut Vec<u8>,
    total: &mut u32,
) {
    let mut x = rect.x;
    while x < rect.right() {
        add_theme_sample(image.get_pixel(x, y).0, dark_samples, light_samples, total);
        x = x.saturating_add(step);
    }
}

fn sample_theme_column(
    image: &RgbaImage,
    rect: PixelRect,
    x: u32,
    step: u32,
    dark_samples: &mut Vec<u8>,
    light_samples: &mut Vec<u8>,
    total: &mut u32,
) {
    let mut y = rect.y;
    while y < rect.bottom() {
        add_theme_sample(image.get_pixel(x, y).0, dark_samples, light_samples, total);
        y = y.saturating_add(step);
    }
}

fn add_theme_sample(
    pixel: [u8; 4],
    dark_samples: &mut Vec<u8>,
    light_samples: &mut Vec<u8>,
    total: &mut u32,
) {
    *total = total.saturating_add(1);
    if !is_neutral_gray(pixel) {
        return;
    }
    let gray = gray_value(pixel);
    if gray.abs_diff(DARK_TEAMS_BACKGROUND) <= 28 {
        dark_samples.push(gray);
    }
    if gray.abs_diff(LIGHT_TEAMS_BACKGROUND) <= 28 {
        light_samples.push(gray);
    }
}

fn background_row_run(
    image: &RgbaImage,
    rect: PixelRect,
    background: [u8; 4],
    max_trim: u32,
    from_start: bool,
) -> u32 {
    let mut run = 0u32;
    for offset in 0..max_trim.min(rect.height.saturating_sub(1)) {
        let y = if from_start {
            rect.y + offset
        } else {
            rect.bottom() - 1 - offset
        };
        if line_matches_background(image, y, rect.x, rect.width, true, background) {
            run += 1;
        } else {
            break;
        }
    }
    run
}

fn background_column_run(
    image: &RgbaImage,
    rect: PixelRect,
    background: [u8; 4],
    max_trim: u32,
    from_start: bool,
) -> u32 {
    let mut run = 0u32;
    for offset in 0..max_trim.min(rect.width.saturating_sub(1)) {
        let x = if from_start {
            rect.x + offset
        } else {
            rect.right() - 1 - offset
        };
        if line_matches_background(image, x, rect.y, rect.height, false, background) {
            run += 1;
        } else {
            break;
        }
    }
    run
}

fn line_matches_background(
    image: &RgbaImage,
    fixed: u32,
    start: u32,
    length: u32,
    horizontal: bool,
    background: [u8; 4],
) -> bool {
    if length == 0 {
        return false;
    }
    let step = (length / 256).max(1);
    let mut samples = 0u32;
    let mut matches = 0u32;
    let mut offset = 0u32;
    while offset < length {
        let pixel = if horizontal {
            image.get_pixel(start + offset, fixed).0
        } else {
            image.get_pixel(fixed, start + offset).0
        };
        samples += 1;
        if is_neutral_gray(pixel) && color_distance(pixel, background) <= 4.0 {
            matches += 1;
        }
        offset = offset.saturating_add(step);
    }
    samples > 0 && matches.saturating_mul(100) >= samples.saturating_mul(91)
}

fn is_near_known_background(gray: u8, tolerance: u8) -> bool {
    gray.abs_diff(DARK_TEAMS_BACKGROUND) <= tolerance
        || gray.abs_diff(LIGHT_TEAMS_BACKGROUND) <= tolerance
}

fn is_neutral_gray(pixel: [u8; 4]) -> bool {
    pixel[0].max(pixel[1]).max(pixel[2]) - pixel[0].min(pixel[1]).min(pixel[2]) <= 24
}

fn gray_value(pixel: [u8; 4]) -> u8 {
    ((u16::from(pixel[0]) + u16::from(pixel[1]) + u16::from(pixel[2])) / 3) as u8
}

fn color_distance(left: [u8; 4], right: [u8; 4]) -> f32 {
    (i16::from(left[0]).abs_diff(i16::from(right[0]))
        + i16::from(left[1]).abs_diff(i16::from(right[1]))
        + i16::from(left[2]).abs_diff(i16::from(right[2]))) as f32
        / 3.0
}

#[cfg(test)]
mod tests {
    use image::{Rgba, RgbaImage};

    use super::refine_teams_rect;
    use crate::capture::content_detector::PixelRect;

    #[test]
    fn dark_vertical_triple_border_excludes_participants() {
        let mut image = RgbaImage::from_pixel(1600, 900, Rgba([0x47, 0x47, 0x47, 255]));
        let shared = PixelRect::new(90, 72, 1120, 792);
        draw_dark_window(&mut image, shared);
        draw_vertical_separator(&mut image, 1240, 0xa8, 0x9b);
        draw_participant_column(&mut image, 1250, 330);

        let detected = refine_teams_rect(&image, PixelRect::new(0, 0, 1600, 900));
        assert!(detected.x >= 75 && detected.x <= 105, "{detected:?}");
        assert!(
            detected.right() >= 1190 && detected.right() <= 1225,
            "{detected:?}"
        );
        assert!(detected.bottom() >= 850, "{detected:?}");
    }

    #[test]
    fn light_horizontal_triple_border_excludes_top_strip() {
        let mut image = RgbaImage::from_pixel(1440, 900, Rgba([0xb2, 0xb2, 0xb2, 255]));
        for column in 0..3 {
            fill_rect(
                &mut image,
                PixelRect::new(220 + column * 350, 24, 300, 130),
                Rgba([185, 184, 186, 255]),
            );
        }
        draw_horizontal_separator(&mut image, 170, 0x72, 0x60);
        let shared = PixelRect::new(120, 184, 1200, 690);
        draw_light_window(&mut image, shared);

        let detected = refine_teams_rect(&image, PixelRect::new(0, 0, 1440, 900));
        assert!(detected.y >= 175 && detected.y <= 195, "{detected:?}");
        assert!(detected.x >= 105 && detected.x <= 135, "{detected:?}");
        assert!(detected.bottom() >= 860, "{detected:?}");
    }

    #[test]
    fn non_fullscreen_dark_window_uses_known_background_margin() {
        let mut image = RgbaImage::from_pixel(1280, 720, Rgba([0x47, 0x47, 0x47, 255]));
        let shared = PixelRect::new(170, 90, 940, 600);
        draw_dark_window(&mut image, shared);

        let detected = refine_teams_rect(&image, PixelRect::new(0, 0, 1280, 720));
        assert_eq!(detected, shared);
    }

    #[test]
    fn shared_taskbar_is_not_trimmed_as_teams_background() {
        let mut image = RgbaImage::from_pixel(1280, 720, Rgba([0x47, 0x47, 0x47, 255]));
        let shared = PixelRect::new(140, 70, 1000, 620);
        draw_dark_window(&mut image, shared);

        let detected = refine_teams_rect(&image, PixelRect::new(0, 0, 1280, 720));
        assert_eq!(detected.bottom(), shared.bottom());
    }

    #[test]
    fn short_internal_three_tone_rule_is_ignored() {
        let mut image = RgbaImage::from_pixel(1280, 720, Rgba([0x47, 0x47, 0x47, 255]));
        let shared = PixelRect::new(120, 70, 1040, 620);
        draw_dark_window(&mut image, shared);
        fill_rect(
            &mut image,
            PixelRect::new(300, 280, 240, 2),
            Rgba([0xa8, 0xa8, 0xa8, 255]),
        );
        fill_rect(
            &mut image,
            PixelRect::new(300, 282, 240, 2),
            Rgba([0x9b, 0x9b, 0x9b, 255]),
        );
        fill_rect(
            &mut image,
            PixelRect::new(300, 284, 240, 2),
            Rgba([0xa8, 0xa8, 0xa8, 255]),
        );

        let detected = refine_teams_rect(&image, PixelRect::new(0, 0, 1280, 720));
        assert_eq!(detected, shared);
    }

    fn draw_vertical_separator(image: &mut RgbaImage, x: u32, outer: u8, inner: u8) {
        let height = image.height();
        fill_rect(
            image,
            PixelRect::new(x, 0, 2, height),
            Rgba([outer, outer, outer, 255]),
        );
        fill_rect(
            image,
            PixelRect::new(x + 2, 0, 2, height),
            Rgba([inner, inner, inner, 255]),
        );
        fill_rect(
            image,
            PixelRect::new(x + 4, 0, 2, height),
            Rgba([outer, outer, outer, 255]),
        );
    }

    fn draw_horizontal_separator(image: &mut RgbaImage, y: u32, outer: u8, inner: u8) {
        let width = image.width();
        fill_rect(
            image,
            PixelRect::new(0, y, width, 2),
            Rgba([outer, outer, outer, 255]),
        );
        fill_rect(
            image,
            PixelRect::new(0, y + 2, width, 2),
            Rgba([inner, inner, inner, 255]),
        );
        fill_rect(
            image,
            PixelRect::new(0, y + 4, width, 2),
            Rgba([outer, outer, outer, 255]),
        );
    }

    fn draw_dark_window(image: &mut RgbaImage, rect: PixelRect) {
        fill_rect(image, rect, Rgba([38, 38, 40, 255]));
        fill_rect(
            image,
            PixelRect::new(rect.x, rect.y, rect.width, 34),
            Rgba([54, 54, 57, 255]),
        );
        fill_rect(
            image,
            PixelRect::new(rect.x + 1, rect.y + 35, rect.width - 2, rect.height - 76),
            Rgba([66, 66, 68, 255]),
        );
        draw_taskbar(
            image,
            PixelRect::new(rect.x, rect.bottom() - 40, rect.width, 40),
        );
    }

    fn draw_light_window(image: &mut RgbaImage, rect: PixelRect) {
        fill_rect(image, rect, Rgba([247, 247, 248, 255]));
        fill_rect(
            image,
            PixelRect::new(rect.x, rect.y, rect.width, 36),
            Rgba([231, 234, 238, 255]),
        );
        fill_rect(
            image,
            PixelRect::new(rect.x + 1, rect.y + 37, rect.width - 2, rect.height - 82),
            Rgba([250, 250, 251, 255]),
        );
        draw_taskbar(
            image,
            PixelRect::new(rect.x, rect.bottom() - 44, rect.width, 44),
        );
    }

    fn draw_taskbar(image: &mut RgbaImage, rect: PixelRect) {
        fill_rect(image, rect, Rgba([31, 31, 34, 255]));
        let icon_size = (rect.height / 2).max(8);
        let start = rect.x + rect.width / 3;
        for index in 0..7 {
            let x = start + index * (icon_size + 8);
            fill_rect(
                image,
                PixelRect::new(
                    x,
                    rect.y + (rect.height - icon_size) / 2,
                    icon_size,
                    icon_size,
                ),
                Rgba([70 + index as u8 * 18, 120, 205, 255]),
            );
        }
    }

    fn draw_participant_column(image: &mut RgbaImage, x: u32, width: u32) {
        for row in 0..3 {
            let tile = PixelRect::new(x + 18, 70 + row * 260, width - 36, 220);
            fill_rect(image, tile, Rgba([61, 58 + row as u8 * 8, 62, 255]));
            fill_rect(
                image,
                PixelRect::new(tile.x + tile.width / 2 - 45, tile.y + 45, 90, 90),
                Rgba([120 + row as u8 * 18, 105, 95, 255]),
            );
        }
    }

    fn fill_rect(image: &mut RgbaImage, rect: PixelRect, color: Rgba<u8>) {
        let right = rect.right().min(image.width());
        let bottom = rect.bottom().min(image.height());
        for y in rect.y..bottom {
            for x in rect.x..right {
                image.put_pixel(x, y, color);
            }
        }
    }
}
