use std::collections::{HashMap, VecDeque};

use image::{RgbaImage, imageops};

use super::{ContentCandidate, PixelRect};

const VISUAL_MAX_WIDTH: u32 = 320;
const VISUAL_MAX_HEIGHT: u32 = 180;
const COLOR_ACTIVITY_THRESHOLD: f32 = 24.0;
const EDGE_ACTIVITY_THRESHOLD: f32 = 20.0;

pub(super) fn detect_visual_candidate(image: &RgbaImage) -> Option<ContentCandidate> {
    let analysis = AnalysisFrame::new(image)?;
    let component = detect_component_candidate(image, &analysis);
    let projection = detect_projection_candidate(image, &analysis);

    match (component, projection) {
        (Some(component), Some(projection)) => {
            if projection.confidence >= component.confidence
                || projection.rect.area() * 100 <= component.rect.area() * 96
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
    distance: Vec<u8>,
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
        let background = estimate_analysis_background(&small);
        let background_luminance = luminance(background);
        let color_activity_threshold = if background_luminance >= 170.0 {
            12.0
        } else if background_luminance >= 125.0 {
            18.0
        } else {
            COLOR_ACTIVITY_THRESHOLD
        };
        let edge_activity_threshold = if background_luminance >= 170.0 {
            10.0
        } else if background_luminance >= 125.0 {
            15.0
        } else {
            EDGE_ACTIVITY_THRESHOLD
        };
        let mut active = vec![false; (width * height) as usize];
        let mut distance = vec![0u8; active.len()];

        for y in 1..height.saturating_sub(1) {
            for x in 1..width.saturating_sub(1) {
                let pixel = small.get_pixel(x, y).0;
                let background_distance = color_distance(pixel, background);
                let edge_strength = local_edge_strength(&small, x, y);
                let pixel_index = index(x, y, width);
                distance[pixel_index] = background_distance.round().clamp(0.0, 255.0) as u8;
                active[pixel_index] = background_distance >= color_activity_threshold
                    || (background_distance >= color_activity_threshold * 0.35
                        && edge_strength >= edge_activity_threshold);
            }
        }

        // Opening removes isolated UI noise without joining participant tiles to the
        // shared surface across a Teams gutter.
        let active = dilate(&erode(&active, width, height), width, height);
        Some(Self {
            width,
            height,
            active,
            distance,
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

    fn mean_distance(&self, rect: PixelRect) -> f32 {
        let right = rect.right().min(self.width);
        let bottom = rect.bottom().min(self.height);
        let mut count = 0u64;
        let mut sum = 0u64;
        for y in rect.y..bottom {
            for x in rect.x..right {
                sum += u64::from(self.distance[index(x, y, self.width)]);
                count += 1;
            }
        }
        if count == 0 {
            0.0
        } else {
            sum as f32 / count as f32
        }
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
        let candidate = ContentCandidate::new(refine_projected_edges(image, rect), confidence);
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
    let mut column_counts = vec![0u32; analysis.width as usize];
    for y in 0..analysis.height {
        for x in 0..analysis.width {
            if analysis.active[index(x, y, analysis.width)] {
                column_counts[x as usize] += 1;
            }
        }
    }

    let smoothed_columns = smooth_counts(&column_counts, 2);
    let max_column_ratio =
        smoothed_columns.iter().copied().fold(0.0_f32, f32::max) / analysis.height as f32;
    if max_column_ratio < 0.25 {
        return None;
    }
    let column_threshold = (max_column_ratio * 0.72).clamp(0.30, 0.78);
    let columns = best_dense_run(
        &smoothed_columns,
        analysis.height,
        column_threshold,
        (analysis.width / 8).max(3),
    )?;

    let column_width = columns.end.saturating_sub(columns.start);
    if column_width == 0 {
        return None;
    }
    let mut row_counts = vec![0u32; analysis.height as usize];
    for y in 0..analysis.height {
        let mut count = 0u32;
        for x in columns.start..columns.end {
            count += u32::from(analysis.active[index(x, y, analysis.width)]);
        }
        row_counts[y as usize] = count;
    }

    let smoothed_rows = smooth_counts(&row_counts, 1);
    let max_row_ratio = smoothed_rows.iter().copied().fold(0.0_f32, f32::max) / column_width as f32;
    if max_row_ratio < 0.25 {
        return None;
    }
    let row_threshold = (max_row_ratio * 0.70).clamp(0.30, 0.78);
    let mut rows = best_dense_run(
        &smoothed_rows,
        column_width,
        row_threshold,
        (analysis.height / 8).max(3),
    )?;

    if rows.start <= (analysis.height / 20).max(4) {
        rows.start = 0;
    }
    rows.end = extend_bottom_over_desktop_chrome(analysis, columns, rows, &row_counts);

    let small_rect = PixelRect::new(
        columns.start,
        rows.start,
        columns.end.saturating_sub(columns.start),
        rows.end.saturating_sub(rows.start),
    );
    if small_rect.width == 0 || small_rect.height == 0 {
        return None;
    }

    let small_area = small_rect.area();
    let image_area = u64::from(analysis.width) * u64::from(analysis.height);
    let area_ratio = small_area as f32 / image_area as f32;
    let aspect_ratio = small_rect.width as f32 / small_rect.height as f32;
    if !(0.12..=0.97).contains(&area_ratio) || !(0.30..=6.5).contains(&aspect_ratio) {
        return None;
    }

    let fill_ratio = analysis.active_count(small_rect) as f32 / small_area as f32;
    if fill_ratio < 0.48 {
        return None;
    }

    let rect = scale_rect(
        small_rect,
        image.width(),
        image.height(),
        analysis.width,
        analysis.height,
    )?;
    let rect = refine_projected_edges(image, rect);
    let confidence = 0.50
        + fill_ratio.min(1.0) * 0.20
        + rows.mean_ratio.min(1.0) * 0.10
        + columns.mean_ratio.min(1.0) * 0.12
        + (area_ratio / 0.55).min(1.0) * 0.08;
    Some(ContentCandidate::new(rect, confidence))
}

fn extend_bottom_over_desktop_chrome(
    analysis: &AnalysisFrame,
    columns: DenseRun,
    rows: DenseRun,
    row_counts: &[u32],
) -> u32 {
    let column_width = columns.end.saturating_sub(columns.start);
    if column_width == 0 || rows.end >= analysis.height {
        return rows.end;
    }

    let max_extension = (analysis.height / 10).max(6);
    let scan_end = rows.end.saturating_add(max_extension).min(analysis.height);
    let initial_gap_limit = (analysis.height / 36).clamp(3, 8);
    let trailing_allowance = (analysis.height / 72).clamp(1, 3);
    let mut last_structured = rows.end;
    let mut background_gap = 0u32;
    let mut saw_structured = false;

    for y in rows.end..scan_end {
        let active_ratio = row_counts[y as usize] as f32 / column_width as f32;
        let mean_distance =
            analysis.mean_distance(PixelRect::new(columns.start, y, column_width, 1));
        let structured = active_ratio >= 0.02 || mean_distance >= 10.0;
        if structured {
            saw_structured = true;
            last_structured = y + 1;
            background_gap = 0;
        } else {
            background_gap += 1;
            let gap_limit = if saw_structured {
                trailing_allowance
            } else {
                initial_gap_limit
            };
            if background_gap > gap_limit {
                break;
            }
        }
    }

    if saw_structured {
        last_structured
            .saturating_add(trailing_allowance)
            .min(scan_end)
    } else {
        rows.end
    }
}

#[derive(Clone, Copy)]
struct DenseRun {
    start: u32,
    end: u32,
    mean_ratio: f32,
}

fn smooth_counts(counts: &[u32], radius: usize) -> Vec<f32> {
    if counts.is_empty() {
        return Vec::new();
    }
    let mut output = vec![0.0; counts.len()];
    for (index, value) in output.iter_mut().enumerate() {
        let start = index.saturating_sub(radius);
        let end = (index + radius + 1).min(counts.len());
        let sum: u64 = counts[start..end]
            .iter()
            .map(|count| u64::from(*count))
            .sum();
        *value = sum as f32 / (end - start) as f32;
    }
    output
}

fn best_dense_run(
    counts: &[f32],
    denominator: u32,
    threshold: f32,
    min_length: u32,
) -> Option<DenseRun> {
    if counts.is_empty() || denominator == 0 {
        return None;
    }

    let mut dense: Vec<bool> = counts
        .iter()
        .map(|count| *count / denominator as f32 >= threshold)
        .collect();
    close_short_gaps(&mut dense, 2);

    let mut best: Option<(f32, DenseRun)> = None;
    let mut index = 0usize;
    while index < dense.len() {
        if !dense[index] {
            index += 1;
            continue;
        }
        let start = index;
        while index < dense.len() && dense[index] {
            index += 1;
        }
        let end = index;
        let length = end.saturating_sub(start) as u32;
        if length < min_length {
            continue;
        }

        let sum: f32 = counts[start..end].iter().sum();
        let mean_ratio = sum / (length as f32 * denominator as f32);
        let center = (start + end) as f32 / 2.0 / dense.len() as f32;
        let centrality = (1.0 - (center - 0.5).abs() * 1.3).clamp(0.0, 1.0);
        let score = length as f32 * mean_ratio * (0.80 + centrality * 0.20);
        let run = DenseRun {
            start: start as u32,
            end: end as u32,
            mean_ratio,
        };
        if best.is_none_or(|(best_score, _)| score > best_score) {
            best = Some((score, run));
        }
    }

    best.map(|(_, run)| run)
}

fn close_short_gaps(mask: &mut [bool], max_gap: usize) {
    let mut index = 0usize;
    while index < mask.len() {
        if mask[index] {
            index += 1;
            continue;
        }
        let start = index;
        while index < mask.len() && !mask[index] {
            index += 1;
        }
        if start > 0 && index < mask.len() && index - start <= max_gap {
            mask[start..index].fill(true);
        }
    }
}

pub(super) fn refine_uniform_margins(image: &RgbaImage, rect: PixelRect) -> PixelRect {
    let background = estimate_outer_background(image);
    let max_vertical_trim = (rect.height / 8).min(80);
    let max_horizontal_trim = (rect.width / 8).min(80);

    let top_trim = background_row_run(image, rect, background, max_vertical_trim, true);
    let preserve_bottom = looks_like_desktop_taskbar(image, rect);
    let bottom_trim = if preserve_bottom {
        0
    } else {
        background_row_run(image, rect, background, max_vertical_trim, false)
    };
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
    if !(0.20..=0.97).contains(&area_ratio) {
        return rect;
    }

    let tolerance_x = (rect.width / 30).max(6);
    let tolerance_y = (rect.height / 30).max(6);
    let left_aligned = surface.rect.x.abs_diff(rect.x) <= tolerance_x;
    let right_aligned = surface.rect.right().abs_diff(rect.right()) <= tolerance_x;
    let top_aligned = surface.rect.y.abs_diff(rect.y) <= tolerance_y;
    let bottom_aligned = surface.rect.bottom().abs_diff(rect.bottom()) <= tolerance_y;
    let opposite_edges_aligned = (left_aligned && right_aligned) || (top_aligned && bottom_aligned);
    let removes_side_band = surface.rect.width * 100 <= rect.width * 94
        && surface.rect.height * 100 >= rect.height * 72;
    let removes_top_band = surface.rect.height * 100 <= rect.height * 94
        && surface.rect.width * 100 >= rect.width * 72;

    if opposite_edges_aligned || removes_side_band || removes_top_band {
        surface.rect
    } else {
        rect
    }
}

fn refine_projected_edges(image: &RgbaImage, rect: PixelRect) -> PixelRect {
    if image.width() < 2 || image.height() < 2 {
        return rect;
    }

    let radius_x = (image.width() / 64).clamp(8, 40);
    let radius_y = (image.height() / 64).clamp(6, 28);
    let left = if rect.x <= radius_x {
        0
    } else {
        strongest_vertical_boundary(image, rect.x, radius_x, rect.y, rect.bottom())
            .filter(|(_, strength)| *strength >= 6.0)
            .map_or(rect.x, |(position, _)| position)
    };
    let right_gap = image.width().saturating_sub(rect.right());
    let right = if right_gap <= radius_x {
        image.width()
    } else {
        strongest_vertical_boundary(image, rect.right(), radius_x, rect.y, rect.bottom())
            .filter(|(_, strength)| *strength >= 6.0)
            .map_or(rect.right(), |(position, _)| position)
    };
    let top = if rect.y <= radius_y {
        0
    } else {
        strongest_horizontal_boundary(image, rect.y, radius_y, left, right)
            .filter(|(_, strength)| *strength >= 4.0)
            .map_or(rect.y, |(position, _)| position)
    };
    let bottom_gap = image.height().saturating_sub(rect.bottom());
    let bottom = if bottom_gap <= radius_y {
        image.height()
    } else {
        strongest_horizontal_boundary(image, rect.bottom(), radius_y, left, right)
            .filter(|(_, strength)| *strength >= 4.0)
            .map_or(rect.bottom(), |(position, _)| position)
    };

    if right > left && bottom > top {
        PixelRect::new(left, top, right - left, bottom - top)
    } else {
        rect
    }
}

fn strongest_vertical_boundary(
    image: &RgbaImage,
    center: u32,
    radius: u32,
    top: u32,
    bottom: u32,
) -> Option<(u32, f32)> {
    let start = center.saturating_sub(radius).max(1);
    let end = center
        .saturating_add(radius)
        .min(image.width().saturating_sub(1));
    if start > end || bottom <= top {
        return None;
    }

    let mut best = None;
    for x in start..=end {
        let strength = vertical_boundary_strength(image, x, top, bottom);
        if best.is_none_or(|(_, best_strength)| strength > best_strength) {
            best = Some((x, strength));
        }
    }
    best
}

fn strongest_horizontal_boundary(
    image: &RgbaImage,
    center: u32,
    radius: u32,
    left: u32,
    right: u32,
) -> Option<(u32, f32)> {
    let start = center.saturating_sub(radius).max(1);
    let end = center
        .saturating_add(radius)
        .min(image.height().saturating_sub(1));
    if start > end || right <= left {
        return None;
    }

    let mut best = None;
    for y in start..=end {
        let strength = horizontal_boundary_strength(image, y, left, right);
        if best.is_none_or(|(_, best_strength)| strength > best_strength) {
            best = Some((y, strength));
        }
    }
    best
}

fn vertical_boundary_strength(image: &RgbaImage, x: u32, top: u32, bottom: u32) -> f32 {
    if x == 0 || x >= image.width() || bottom <= top {
        return 0.0;
    }
    let step = ((bottom - top) / 256).max(1);
    let mut samples = 0u32;
    let mut sum = 0.0;
    let mut y = top;
    while y < bottom.min(image.height()) {
        sum += color_distance(image.get_pixel(x - 1, y).0, image.get_pixel(x, y).0);
        samples += 1;
        y = y.saturating_add(step);
    }
    if samples == 0 {
        0.0
    } else {
        sum / samples as f32
    }
}

fn horizontal_boundary_strength(image: &RgbaImage, y: u32, left: u32, right: u32) -> f32 {
    if y == 0 || y >= image.height() || right <= left {
        return 0.0;
    }
    let step = ((right - left) / 256).max(1);
    let mut samples = 0u32;
    let mut sum = 0.0;
    let mut x = left;
    while x < right.min(image.width()) {
        sum += color_distance(image.get_pixel(x, y - 1).0, image.get_pixel(x, y).0);
        samples += 1;
        x = x.saturating_add(step);
    }
    if samples == 0 {
        0.0
    } else {
        sum / samples as f32
    }
}

fn looks_like_desktop_taskbar(image: &RgbaImage, rect: PixelRect) -> bool {
    if rect.width < 240 || rect.height < 160 {
        return false;
    }
    let band_height = (rect.height / 18).clamp(20, 64).min(rect.height / 4);
    if band_height == 0 {
        return false;
    }
    let band = PixelRect::new(rect.x, rect.bottom() - band_height, rect.width, band_height);
    let base = estimate_region_mode(image, band);
    let step_x = (band.width / 512).max(1);
    let step_y = (band.height / 24).max(1);
    let mut samples = 0u32;
    let mut distinctive = 0u32;

    let mut y = band.y;
    while y < band.bottom() {
        let mut x = band.x;
        while x < band.right() {
            let pixel = image.get_pixel(x, y).0;
            let chroma =
                pixel[0].max(pixel[1]).max(pixel[2]) - pixel[0].min(pixel[1]).min(pixel[2]);
            if color_distance(pixel, base) >= 24.0 || chroma >= 36 {
                distinctive += 1;
            }
            samples += 1;
            x = x.saturating_add(step_x);
        }
        y = y.saturating_add(step_y);
    }

    if samples == 0 {
        return false;
    }
    let distinctive_ratio = distinctive as f32 / samples as f32;
    let search_padding = (band.height / 2).max(4);
    let search_start = band
        .y
        .saturating_sub(search_padding)
        .max(rect.y.saturating_add(1));
    let search_end = band
        .bottom()
        .min(rect.bottom())
        .min(image.height().saturating_sub(1));
    let boundary_strength = if search_start <= search_end {
        (search_start..=search_end)
            .map(|y| horizontal_boundary_strength(image, y, band.x, band.right()))
            .fold(0.0_f32, f32::max)
    } else {
        0.0
    };
    (0.003..=0.40).contains(&distinctive_ratio) && boundary_strength >= 4.0
}

#[derive(Default)]
struct ColorBucket {
    count: u32,
    red: u64,
    green: u64,
    blue: u64,
    sides: u8,
}

impl ColorBucket {
    fn add(&mut self, pixel: [u8; 4], side: u8) {
        self.count += 1;
        self.red += u64::from(pixel[0]);
        self.green += u64::from(pixel[1]);
        self.blue += u64::from(pixel[2]);
        self.sides |= side;
    }

    fn color(&self) -> [u8; 4] {
        if self.count == 0 {
            return [0, 0, 0, 255];
        }
        [
            (self.red / u64::from(self.count)) as u8,
            (self.green / u64::from(self.count)) as u8,
            (self.blue / u64::from(self.count)) as u8,
            255,
        ]
    }
}

fn estimate_analysis_background(image: &RgbaImage) -> [u8; 4] {
    estimate_background_from_perimeter(image)
}

fn estimate_outer_background(image: &RgbaImage) -> [u8; 4] {
    estimate_background_from_perimeter(image)
}

fn estimate_background_from_perimeter(image: &RgbaImage) -> [u8; 4] {
    let width = image.width();
    let height = image.height();
    if width == 0 || height == 0 {
        return [0, 0, 0, 255];
    }

    let band = (width.min(height) / 40).clamp(1, 8);
    let step = (width.max(height) / 1024).max(1);
    let mut buckets = HashMap::<u16, ColorBucket>::new();

    for y in 0..band {
        sample_horizontal_edge(image, y, step, 0b0001, &mut buckets);
    }
    for y in height.saturating_sub(band)..height {
        sample_horizontal_edge(image, y, step, 0b0010, &mut buckets);
    }
    for x in 0..band {
        sample_vertical_edge(
            image,
            x,
            band,
            height.saturating_sub(band),
            step,
            0b0100,
            &mut buckets,
        );
    }
    for x in width.saturating_sub(band)..width {
        sample_vertical_edge(
            image,
            x,
            band,
            height.saturating_sub(band),
            step,
            0b1000,
            &mut buckets,
        );
    }

    choose_background_bucket(&buckets)
}

fn sample_horizontal_edge(
    image: &RgbaImage,
    y: u32,
    step: u32,
    side: u8,
    buckets: &mut HashMap<u16, ColorBucket>,
) {
    let mut x = 0u32;
    while x < image.width() {
        let pixel = image.get_pixel(x, y).0;
        buckets
            .entry(color_bucket_key(pixel))
            .or_default()
            .add(pixel, side);
        x = x.saturating_add(step);
    }
}

fn sample_vertical_edge(
    image: &RgbaImage,
    x: u32,
    start: u32,
    end: u32,
    step: u32,
    side: u8,
    buckets: &mut HashMap<u16, ColorBucket>,
) {
    let mut y = start;
    while y < end {
        let pixel = image.get_pixel(x, y).0;
        buckets
            .entry(color_bucket_key(pixel))
            .or_default()
            .add(pixel, side);
        y = y.saturating_add(step);
    }
}

fn choose_background_bucket(buckets: &HashMap<u16, ColorBucket>) -> [u8; 4] {
    if buckets.is_empty() {
        return [0, 0, 0, 255];
    }
    let total: u32 = buckets.values().map(|bucket| bucket.count).sum();
    let mut best: Option<(f32, &ColorBucket)> = None;

    for bucket in buckets.values() {
        let color = bucket.color();
        let chroma = color[0].max(color[1]).max(color[2]) - color[0].min(color[1]).min(color[2]);
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

fn estimate_region_mode(image: &RgbaImage, rect: PixelRect) -> [u8; 4] {
    let mut buckets = HashMap::<u16, ColorBucket>::new();
    let step_x = (rect.width / 256).max(1);
    let step_y = (rect.height / 32).max(1);
    let mut y = rect.y;
    while y < rect.bottom().min(image.height()) {
        let mut x = rect.x;
        while x < rect.right().min(image.width()) {
            let pixel = image.get_pixel(x, y).0;
            buckets
                .entry(color_bucket_key(pixel))
                .or_default()
                .add(pixel, 0);
            x = x.saturating_add(step_x);
        }
        y = y.saturating_add(step_y);
    }
    buckets
        .values()
        .max_by_key(|bucket| bucket.count)
        .map_or([0, 0, 0, 255], ColorBucket::color)
}

fn color_bucket_key(pixel: [u8; 4]) -> u16 {
    (u16::from(pixel[0] >> 3) << 10) | (u16::from(pixel[1] >> 3) << 5) | u16::from(pixel[2] >> 3)
}

fn local_edge_strength(image: &RgbaImage, x: u32, y: u32) -> f32 {
    let pixel = image.get_pixel(x, y).0;
    [
        image.get_pixel(x - 1, y).0,
        image.get_pixel(x + 1, y).0,
        image.get_pixel(x, y - 1).0,
        image.get_pixel(x, y + 1).0,
    ]
    .into_iter()
    .map(|neighbor| color_distance(pixel, neighbor))
    .fold(0.0_f32, f32::max)
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

fn luminance(pixel: [u8; 4]) -> f32 {
    (f32::from(pixel[0]) * 54.0 + f32::from(pixel[1]) * 183.0 + f32::from(pixel[2]) * 19.0) / 256.0
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
    (right > left && bottom > top).then_some(PixelRect::new(left, top, right - left, bottom - top))
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
    fn projection_removes_top_participant_strip_and_keeps_taskbar() {
        let mut image = RgbaImage::from_pixel(1600, 900, Rgba([28, 28, 31, 255]));
        fill_rect(
            &mut image,
            PixelRect::new(0, 180, 1600, 720),
            Rgba([238, 240, 244, 255]),
        );
        fill_rect(
            &mut image,
            PixelRect::new(0, 860, 1600, 40),
            Rgba([35, 35, 38, 255]),
        );
        add_taskbar_icons(&mut image, PixelRect::new(0, 860, 1600, 40));
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
        assert_eq!(detected.bottom(), 900, "{detected:?}");
    }

    #[test]
    fn large_side_participant_strip_is_removed_and_taskbar_is_kept() {
        let mut image = RgbaImage::from_pixel(2048, 740, Rgba([28, 28, 31, 255]));
        let shared = PixelRect::new(286, 0, 1198, 740);
        fill_rect(&mut image, shared, Rgba([238, 240, 244, 255]));
        fill_rect(
            &mut image,
            PixelRect::new(shared.x, 300, shared.width, 408),
            Rgba([9, 54, 113, 255]),
        );
        fill_rect(
            &mut image,
            PixelRect::new(shared.x, 708, shared.width, 32),
            Rgba([35, 35, 38, 255]),
        );
        add_taskbar_icons(&mut image, PixelRect::new(shared.x, 708, shared.width, 32));
        for row in 0..3 {
            fill_rect(
                &mut image,
                PixelRect::new(1772, 140 + row * 155, 276, 150),
                Rgba([62, 58 + row as u8 * 10, 60, 255]),
            );
            fill_rect(
                &mut image,
                PixelRect::new(1870, 175 + row * 155, 72, 72),
                Rgba([145, 105, 92, 255]),
            );
        }

        let detected = refine_uniform_margins(&image, PixelRect::new(0, 0, 2048, 740));
        assert!(detected.x >= 270 && detected.x <= 300, "{detected:?}");
        assert!(
            detected.right() >= 1465 && detected.right() <= 1500,
            "{detected:?}"
        );
        assert_eq!(detected.y, 0, "{detected:?}");
        assert_eq!(detected.bottom(), 740, "{detected:?}");
    }

    #[test]
    fn projection_stops_after_taskbar_before_teams_margin() {
        let mut image = RgbaImage::from_pixel(1600, 900, Rgba([28, 28, 31, 255]));
        let shared = PixelRect::new(160, 70, 1080, 760);
        fill_rect(&mut image, shared, Rgba([238, 240, 244, 255]));
        fill_rect(
            &mut image,
            PixelRect::new(160, 790, 1080, 40),
            Rgba([35, 35, 38, 255]),
        );
        add_taskbar_icons(&mut image, PixelRect::new(160, 790, 1080, 40));
        for row in 0..3 {
            fill_rect(
                &mut image,
                PixelRect::new(1320, 90 + row * 245, 240, 220),
                Rgba([68, 62 + row as u8 * 12, 66, 255]),
            );
        }

        let candidate = detect_visual_candidate(&image).expect("shared surface should be found");
        assert!(
            candidate.rect.x >= 140 && candidate.rect.x <= 180,
            "{candidate:?}"
        );
        assert!(candidate.rect.right() <= 1260, "{candidate:?}");
        assert!(candidate.rect.bottom() >= 815, "{candidate:?}");
        assert!(candidate.rect.bottom() <= 845, "{candidate:?}");
    }

    #[test]
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

    #[test]
    fn uniform_dark_content_is_not_shrunk_without_a_boundary() {
        let image = RgbaImage::from_pixel(1280, 720, Rgba([24, 24, 28, 255]));
        let rect = PixelRect::new(100, 80, 1000, 560);
        assert_eq!(refine_uniform_margins(&image, rect), rect);
    }

    fn add_taskbar_icons(image: &mut RgbaImage, taskbar: PixelRect) {
        let icon_size = (taskbar.height / 2).max(8);
        let start = taskbar.x + taskbar.width / 4;
        let gap = icon_size + (icon_size / 2).max(4);
        for index in 0..8 {
            let x = start + index * gap;
            if x + icon_size >= taskbar.right() {
                break;
            }
            fill_rect(
                image,
                PixelRect::new(
                    x,
                    taskbar.y + (taskbar.height - icon_size) / 2,
                    icon_size,
                    icon_size,
                ),
                Rgba([80 + index as u8 * 12, 125, 210, 255]),
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
