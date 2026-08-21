use std::collections::VecDeque;

use image::{RgbaImage, imageops};

use super::{ContentCandidate, PixelRect};

const VISUAL_MAX_WIDTH: u32 = 240;
const VISUAL_MAX_HEIGHT: u32 = 150;

pub(super) fn detect_visual_candidate(image: &RgbaImage) -> Option<ContentCandidate> {
    let analysis = AnalysisFrame::new(image)?;
    let component = detect_component_candidate(image, &analysis);
    let projection = detect_projection_candidate(image, &analysis);

    match (component, projection) {
        (Some(component), Some(projection)) => {
            if projection.confidence >= component.confidence
                || projection.rect.area() * 100 <= component.rect.area() * 92
            {
                Some(projection)
            } else {
                Some(component)
            }
        }
        (Some(candidate), None) | (None, Some(candidate)) => Some(candidate),
        (None, None) => None,
    }
}

struct AnalysisFrame {
    width: u32,
    height: u32,
    active: Vec<bool>,
}

impl AnalysisFrame {
    fn new(image: &RgbaImage) -> Option<Self> {
        if image.width() < 160 || image.height() < 90 {
            return None;
        }

        let scale = (VISUAL_MAX_WIDTH as f32 / image.width() as f32)
            .min(VISUAL_MAX_HEIGHT as f32 / image.height() as f32)
            .min(1.0);
        let width = ((image.width() as f32 * scale).round() as u32).max(1);
        let height = ((image.height() as f32 * scale).round() as u32).max(1);
        let small = imageops::resize(image, width, height, imageops::FilterType::Triangle);
        let background = estimate_outer_background(&small);
        let mut active = vec![false; (width * height) as usize];

        for y in 1..height.saturating_sub(1) {
            for x in 1..width.saturating_sub(1) {
                let pixel = small.get_pixel(x, y).0;
                active[index(x, y, width)] = color_distance(pixel, background) >= 24.0;
            }
        }

        // Opening removes isolated UI noise without joining a nearby participant panel to
        // the shared surface.
        let active = dilate(&erode(&active, width, height), width, height);
        Some(Self {
            width,
            height,
            active,
        })
    }

    fn active_count(&self, rect: PixelRect) -> u32 {
        let right = rect.right().min(self.width);
        let bottom = rect.bottom().min(self.height);
        let mut count = 0u32;
        for y in rect.y..bottom {
            for x in rect.x..right {
                count += u32::from(self.active[index(x, y, self.width)]);
            }
        }
        count
    }
}

fn detect_component_candidate(
    image: &RgbaImage,
    analysis: &AnalysisFrame,
) -> Option<ContentCandidate> {
    let components = connected_components(&analysis.active, analysis.width, analysis.height);
    let mut best: Option<ContentCandidate> = None;

    for component in components {
        let width = component.max_x - component.min_x + 1;
        let height = component.max_y - component.min_y + 1;
        let area = u64::from(width) * u64::from(height);
        let image_area = u64::from(analysis.width) * u64::from(analysis.height);
        let area_ratio = area as f32 / image_area as f32;
        let fill_ratio = component.pixel_count as f32 / area as f32;
        let aspect_ratio = width as f32 / height as f32;

        if !(0.12..=0.86).contains(&area_ratio)
            || fill_ratio < 0.72
            || !(0.35..=6.0).contains(&aspect_ratio)
        {
            continue;
        }

        let edge_margin_x = (analysis.width / 40).max(1);
        let edge_margin_y = (analysis.height / 40).max(1);
        if component.min_x <= edge_margin_x
            || component.min_y <= edge_margin_y
            || component.max_x + edge_margin_x >= analysis.width.saturating_sub(1)
            || component.max_y + edge_margin_y >= analysis.height.saturating_sub(1)
        {
            continue;
        }

        let center_x = (component.min_x + component.max_x + 1) as f32 / 2.0;
        let center_y = (component.min_y + component.max_y + 1) as f32 / 2.0;
        let normalized_dx = ((center_x / analysis.width as f32) - 0.5).abs() * 2.0;
        let normalized_dy = ((center_y / analysis.height as f32) - 0.5).abs() * 2.0;
        let centrality = 1.0
            - ((normalized_dx * normalized_dx + normalized_dy * normalized_dy).sqrt()
                / 2.0_f32.sqrt())
            .clamp(0.0, 1.0);
        let confidence = 0.42
            + fill_ratio.min(1.0) * 0.24
            + centrality * 0.18
            + (area_ratio / 0.60).min(1.0) * 0.16;

        let rect = scale_rect(
            PixelRect::new(component.min_x, component.min_y, width, height),
            image.width(),
            image.height(),
            analysis.width,
            analysis.height,
        )?;
        let candidate = ContentCandidate::new(rect, confidence);
        if best.is_none_or(|current| candidate.confidence > current.confidence) {
            best = Some(candidate);
        }
    }

    best
}

fn detect_projection_candidate(
    image: &RgbaImage,
    analysis: &AnalysisFrame,
) -> Option<ContentCandidate> {
    let mut row_counts = vec![0u32; analysis.height as usize];
    let mut column_counts = vec![0u32; analysis.width as usize];
    for y in 0..analysis.height {
        for x in 0..analysis.width {
            if analysis.active[index(x, y, analysis.width)] {
                row_counts[y as usize] += 1;
                column_counts[x as usize] += 1;
            }
        }
    }

    let max_row_ratio = row_counts
        .iter()
        .copied()
        .max()
        .unwrap_or_default() as f32
        / analysis.width as f32;
    let max_column_ratio = column_counts
        .iter()
        .copied()
        .max()
        .unwrap_or_default() as f32
        / analysis.height as f32;
    if max_row_ratio < 0.32 || max_column_ratio < 0.32 {
        return None;
    }

    let row_threshold = (max_row_ratio * 0.75).clamp(0.38, 0.82);
    let column_threshold = (max_column_ratio * 0.75).clamp(0.38, 0.82);
    let rows = best_dense_run(
        &row_counts,
        analysis.width,
        row_threshold,
        (analysis.height / 8).max(3),
    )?;
    let columns = best_dense_run(
        &column_counts,
        analysis.height,
        column_threshold,
        (analysis.width / 8).max(3),
    )?;
    let small_rect = PixelRect::new(
        columns.start,
        rows.start,
        columns.end - columns.start,
        rows.end - rows.start,
    );
    let small_area = small_rect.area();
    let image_area = u64::from(analysis.width) * u64::from(analysis.height);
    let area_ratio = small_area as f32 / image_area as f32;
    let aspect_ratio = small_rect.width as f32 / small_rect.height as f32;
    if !(0.12..=0.94).contains(&area_ratio) || !(0.30..=6.5).contains(&aspect_ratio) {
        return None;
    }

    let fill_ratio = analysis.active_count(small_rect) as f32 / small_area as f32;
    if fill_ratio < 0.52 {
        return None;
    }

    let rect = scale_rect(
        small_rect,
        image.width(),
        image.height(),
        analysis.width,
        analysis.height,
    )?;
    let confidence = 0.48
        + fill_ratio.min(1.0) * 0.20
        + rows.mean_ratio.min(1.0) * 0.12
        + columns.mean_ratio.min(1.0) * 0.12
        + (area_ratio / 0.55).min(1.0) * 0.08;
    Some(ContentCandidate::new(rect, confidence))
}

#[derive(Clone, Copy)]
struct DenseRun {
    start: u32,
    end: u32,
    mean_ratio: f32,
}

fn best_dense_run(
    counts: &[u32],
    denominator: u32,
    threshold: f32,
    min_length: u32,
) -> Option<DenseRun> {
    if counts.is_empty() || denominator == 0 {
        return None;
    }

    let mut best: Option<(f32, DenseRun)> = None;
    let mut start = None;
    for index in 0..=counts.len() {
        let dense = index < counts.len() && counts[index] as f32 / denominator as f32 >= threshold;
        if dense && start.is_none() {
            start = Some(index);
        }
        if (!dense || index == counts.len()) && start.is_some() {
            let run_start = start.take().unwrap_or_default();
            let run_end = index;
            let length = run_end.saturating_sub(run_start) as u32;
            if length < min_length {
                continue;
            }
            let sum: u64 = counts[run_start..run_end]
                .iter()
                .map(|value| u64::from(*value))
                .sum();
            let mean_ratio = sum as f32 / (length as f32 * denominator as f32);
            let score = length as f32 * mean_ratio;
            let run = DenseRun {
                start: run_start as u32,
                end: run_end as u32,
                mean_ratio,
            };
            if best.is_none_or(|(best_score, _)| score > best_score) {
                best = Some((score, run));
            }
        }
    }
    best.map(|(_, run)| run)
}

pub(super) fn refine_uniform_margins(image: &RgbaImage, rect: PixelRect) -> PixelRect {
    let background = estimate_outer_background(image);
    let max_vertical_trim = (rect.height / 8).min(80);
    let max_horizontal_trim = (rect.width / 8).min(80);

    let top_trim = background_row_run(image, rect, background, max_vertical_trim, true);
    let bottom_trim = background_row_run(image, rect, background, max_vertical_trim, false);
    let vertically_trimmed = PixelRect::new(
        rect.x,
        rect.y.saturating_add(top_trim),
        rect.width,
        rect.height
            .saturating_sub(top_trim.saturating_add(bottom_trim)),
    );

    let left_trim = background_column_run(
        image,
        vertically_trimmed,
        background,
        max_horizontal_trim,
        true,
    );
    let right_trim = background_column_run(
        image,
        vertically_trimmed,
        background,
        max_horizontal_trim,
        false,
    );
    let refined = PixelRect::new(
        vertically_trimmed.x.saturating_add(left_trim),
        vertically_trimmed.y,
        vertically_trimmed
            .width
            .saturating_sub(left_trim.saturating_add(right_trim)),
        vertically_trimmed.height,
    );
    let refined = if refined.area() * 2 >= rect.area() {
        refined
    } else {
        rect
    };

    snap_to_projected_surface(image, refined)
}

fn snap_to_projected_surface(image: &RgbaImage, rect: PixelRect) -> PixelRect {
    let Some(analysis) = AnalysisFrame::new(image) else {
        return rect;
    };
    let Some(surface) = detect_projection_candidate(image, &analysis) else {
        return rect;
    };
    if surface.confidence < 0.86 {
        return rect;
    }
    let Some(intersection) = rect.intersection(surface.rect) else {
        return rect;
    };
    if intersection.area() * 100 < surface.rect.area() * 96 {
        return rect;
    }

    let area_ratio = surface.rect.area() as f32 / rect.area().max(1) as f32;
    if !(0.25..=0.96).contains(&area_ratio) {
        return rect;
    }

    let tolerance_x = (rect.width / 30).max(6);
    let tolerance_y = (rect.height / 30).max(6);
    let left_aligned = surface.rect.x.abs_diff(rect.x) <= tolerance_x;
    let right_aligned = surface.rect.right().abs_diff(rect.right()) <= tolerance_x;
    let top_aligned = surface.rect.y.abs_diff(rect.y) <= tolerance_y;
    let bottom_aligned = surface.rect.bottom().abs_diff(rect.bottom()) <= tolerance_y;
    let opposite_edges_aligned = (left_aligned && right_aligned) || (top_aligned && bottom_aligned);
    let spans_long_axis = surface.rect.width * 100 >= rect.width * 80
        || surface.rect.height * 100 >= rect.height * 80;

    if opposite_edges_aligned || (surface.confidence >= 0.91 && spans_long_axis && area_ratio <= 0.90)
    {
        surface.rect
    } else {
        rect
    }
}

#[derive(Debug)]
struct Component {
    min_x: u32,
    min_y: u32,
    max_x: u32,
    max_y: u32,
    pixel_count: u32,
}

fn connected_components(mask: &[bool], width: u32, height: u32) -> Vec<Component> {
    let mut visited = vec![false; mask.len()];
    let mut components = Vec::new();

    for y in 0..height {
        for x in 0..width {
            let start_index = index(x, y, width);
            if !mask[start_index] || visited[start_index] {
                continue;
            }

            visited[start_index] = true;
            let mut queue = VecDeque::from([(x, y)]);
            let mut component = Component {
                min_x: x,
                min_y: y,
                max_x: x,
                max_y: y,
                pixel_count: 0,
            };

            while let Some((current_x, current_y)) = queue.pop_front() {
                component.min_x = component.min_x.min(current_x);
                component.min_y = component.min_y.min(current_y);
                component.max_x = component.max_x.max(current_x);
                component.max_y = component.max_y.max(current_y);
                component.pixel_count += 1;

                for (next_x, next_y) in neighbors(current_x, current_y, width, height) {
                    let next_index = index(next_x, next_y, width);
                    if mask[next_index] && !visited[next_index] {
                        visited[next_index] = true;
                        queue.push_back((next_x, next_y));
                    }
                }
            }

            components.push(component);
        }
    }

    components
}

fn neighbors(x: u32, y: u32, width: u32, height: u32) -> impl Iterator<Item = (u32, u32)> {
    let mut neighbors = [(0, 0); 4];
    let mut count = 0;
    if x > 0 {
        neighbors[count] = (x - 1, y);
        count += 1;
    }
    if x + 1 < width {
        neighbors[count] = (x + 1, y);
        count += 1;
    }
    if y > 0 {
        neighbors[count] = (x, y - 1);
        count += 1;
    }
    if y + 1 < height {
        neighbors[count] = (x, y + 1);
        count += 1;
    }
    neighbors.into_iter().take(count)
}

fn dilate(mask: &[bool], width: u32, height: u32) -> Vec<bool> {
    let mut output = vec![false; mask.len()];
    for y in 0..height {
        for x in 0..width {
            let mut value = false;
            for sample_y in y.saturating_sub(1)..=(y + 1).min(height.saturating_sub(1)) {
                for sample_x in x.saturating_sub(1)..=(x + 1).min(width.saturating_sub(1)) {
                    value |= mask[index(sample_x, sample_y, width)];
                }
            }
            output[index(x, y, width)] = value;
        }
    }
    output
}

fn erode(mask: &[bool], width: u32, height: u32) -> Vec<bool> {
    let mut output = vec![false; mask.len()];
    for y in 0..height {
        for x in 0..width {
            if x == 0 || y == 0 || x + 1 >= width || y + 1 >= height {
                continue;
            }
            let mut value = true;
            for sample_y in y - 1..=y + 1 {
                for sample_x in x - 1..=x + 1 {
                    value &= mask[index(sample_x, sample_y, width)];
                }
            }
            output[index(x, y, width)] = value;
        }
    }
    output
}

fn background_row_run(
    image: &RgbaImage,
    rect: PixelRect,
    background: [u8; 4],
    max_trim: u32,
    from_start: bool,
) -> u32 {
    if max_trim < 2 || rect.height <= max_trim + 2 {
        return 0;
    }

    let mut run = 0;
    for offset in 0..max_trim {
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

    if run < 2 || run >= max_trim {
        return 0;
    }
    let boundary_y = if from_start {
        rect.y + run
    } else {
        rect.bottom() - 1 - run
    };
    if !line_matches_background(image, boundary_y, rect.x, rect.width, true, background) {
        run
    } else {
        0
    }
}

fn background_column_run(
    image: &RgbaImage,
    rect: PixelRect,
    background: [u8; 4],
    max_trim: u32,
    from_start: bool,
) -> u32 {
    if max_trim < 2 || rect.width <= max_trim + 2 {
        return 0;
    }

    let mut run = 0;
    for offset in 0..max_trim {
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

    if run < 2 || run >= max_trim {
        return 0;
    }
    let boundary_x = if from_start {
        rect.x + run
    } else {
        rect.right() - 1 - run
    };
    if !line_matches_background(image, boundary_x, rect.y, rect.height, false, background) {
        run
    } else {
        0
    }
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
    let mut samples = 0;
    let mut matches = 0;

    let mut offset = 0;
    while offset < length {
        let pixel = if horizontal {
            image.get_pixel(start + offset, fixed).0
        } else {
            image.get_pixel(fixed, start + offset).0
        };
        samples += 1;
        if color_distance(pixel, background) <= 18.0 {
            matches += 1;
        }
        offset = offset.saturating_add(step);
    }

    samples > 0 && matches as f32 / samples as f32 >= 0.92
}

fn estimate_outer_background(image: &RgbaImage) -> [u8; 4] {
    let width = image.width();
    let height = image.height();
    if width == 0 || height == 0 {
        return [0, 0, 0, 255];
    }

    let band = (width.min(height) / 40).clamp(1, 8);
    let mut red = Vec::new();
    let mut green = Vec::new();
    let mut blue = Vec::new();

    let sample_step = (width.max(height) / 1024).max(1);
    for y in 0..band {
        let mut x = 0;
        while x < width {
            push_color_sample(image, x, y, &mut red, &mut green, &mut blue);
            x = x.saturating_add(sample_step);
        }
    }
    for y in height.saturating_sub(band)..height {
        let mut x = 0;
        while x < width {
            push_color_sample(image, x, y, &mut red, &mut green, &mut blue);
            x = x.saturating_add(sample_step);
        }
    }
    for x in 0..band {
        let mut y = band;
        while y < height.saturating_sub(band) {
            push_color_sample(image, x, y, &mut red, &mut green, &mut blue);
            y = y.saturating_add(sample_step);
        }
    }
    for x in width.saturating_sub(band)..width {
        let mut y = band;
        while y < height.saturating_sub(band) {
            push_color_sample(image, x, y, &mut red, &mut green, &mut blue);
            y = y.saturating_add(sample_step);
        }
    }

    red.sort_unstable();
    green.sort_unstable();
    blue.sort_unstable();
    let middle = red.len() / 2;
    [red[middle], green[middle], blue[middle], 255]
}

fn push_color_sample(
    image: &RgbaImage,
    x: u32,
    y: u32,
    red: &mut Vec<u8>,
    green: &mut Vec<u8>,
    blue: &mut Vec<u8>,
) {
    let pixel = image.get_pixel(x, y).0;
    red.push(pixel[0]);
    green.push(pixel[1]);
    blue.push(pixel[2]);
}

fn color_distance(left: [u8; 4], right: [u8; 4]) -> f32 {
    (i16::from(left[0]).abs_diff(i16::from(right[0]))
        + i16::from(left[1]).abs_diff(i16::from(right[1]))
        + i16::from(left[2]).abs_diff(i16::from(right[2]))) as f32
        / 3.0
}

fn scale_rect(
    rect: PixelRect,
    target_width: u32,
    target_height: u32,
    source_width: u32,
    source_height: u32,
) -> Option<PixelRect> {
    let left = scale_floor(rect.x, target_width, source_width);
    let top = scale_floor(rect.y, target_height, source_height);
    let right = scale_ceil(rect.right(), target_width, source_width);
    let bottom = scale_ceil(rect.bottom(), target_height, source_height);
    (right > left && bottom > top).then_some(PixelRect::new(
        left,
        top,
        right - left,
        bottom - top,
    ))
}

fn scale_floor(value: u32, target: u32, source: u32) -> u32 {
    ((u64::from(value) * u64::from(target)) / u64::from(source)) as u32
}

fn scale_ceil(value: u32, target: u32, source: u32) -> u32 {
    (u64::from(value) * u64::from(target))
        .div_ceil(u64::from(source))
        .min(u64::from(target)) as u32
}

fn index(x: u32, y: u32, width: u32) -> usize {
    (y * width + x) as usize
}

#[cfg(test)]
mod tests {
    use image::{Rgba, RgbaImage};

    use super::{detect_visual_candidate, refine_uniform_margins};
    use crate::capture::content_detector::PixelRect;

    #[test]
    fn projection_removes_top_participant_strip() {
        let mut image = RgbaImage::from_pixel(1600, 900, Rgba([28, 28, 31, 255]));
        fill_rect(
            &mut image,
            PixelRect::new(0, 180, 1600, 720),
            Rgba([238, 240, 244, 255]),
        );
        for column in 0..3 {
            fill_rect(
                &mut image,
                PixelRect::new(250 + column * 360, 20, 320, 140),
                Rgba([70 + column as u8 * 18, 62, 66, 255]),
            );
        }

        let detected = refine_uniform_margins(&image, PixelRect::new(0, 0, 1600, 900));
        assert!(detected.y >= 160 && detected.y <= 200, "{detected:?}");
        assert!(detected.width >= 1520, "{detected:?}");
        assert!(detected.bottom() >= 880, "{detected:?}");
    }

    #[test]
    fn projection_removes_side_participant_strip() {
        let mut image = RgbaImage::from_pixel(1600, 900, Rgba([28, 28, 31, 255]));
        fill_rect(
            &mut image,
            PixelRect::new(160, 70, 1080, 760),
            Rgba([238, 240, 244, 255]),
        );
        for row in 0..3 {
            fill_rect(
                &mut image,
                PixelRect::new(1320, 90 + row * 245, 240, 220),
                Rgba([68, 62 + row as u8 * 12, 66, 255]),
            );
        }

        let candidate = detect_visual_candidate(&image).expect("shared surface should be found");
        assert!(candidate.rect.x >= 140 && candidate.rect.x <= 180, "{candidate:?}");
        assert!(candidate.rect.right() <= 1260, "{candidate:?}");
        assert!(candidate.rect.height >= 720, "{candidate:?}");
    }

    #[test]
    fn uniform_dark_content_is_not_shrunk_without_a_boundary() {
        let image = RgbaImage::from_pixel(1280, 720, Rgba([24, 24, 28, 255]));
        let rect = PixelRect::new(100, 80, 1000, 560);
        assert_eq!(refine_uniform_margins(&image, rect), rect);
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
