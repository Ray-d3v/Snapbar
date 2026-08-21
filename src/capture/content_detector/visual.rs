use std::collections::VecDeque;

use image::{RgbaImage, imageops};

use super::{ContentCandidate, PixelRect};

const VISUAL_MAX_WIDTH: u32 = 160;
const VISUAL_MAX_HEIGHT: u32 = 100;

pub(super) fn detect_visual_candidate(image: &RgbaImage) -> Option<ContentCandidate> {
    if image.width() < 160 || image.height() < 90 {
        return None;
    }

    let scale = (VISUAL_MAX_WIDTH as f32 / image.width() as f32)
        .min(VISUAL_MAX_HEIGHT as f32 / image.height() as f32)
        .min(1.0);
    let small_width = ((image.width() as f32 * scale).round() as u32).max(1);
    let small_height = ((image.height() as f32 * scale).round() as u32).max(1);
    let small = imageops::resize(
        image,
        small_width,
        small_height,
        imageops::FilterType::Triangle,
    );
    let background = estimate_outer_background(&small);
    let mut active = vec![false; (small_width * small_height) as usize];

    for y in 1..small_height.saturating_sub(1) {
        for x in 1..small_width.saturating_sub(1) {
            let pixel = small.get_pixel(x, y).0;
            let distance = color_distance(pixel, background);
            active[index(x, y, small_width)] = distance >= 24.0;
        }
    }

    // Opening removes isolated noise without joining a nearby participant/sidebar panel
    // to the shared-content surface.
    let active = dilate(
        &erode(&active, small_width, small_height),
        small_width,
        small_height,
    );
    let components = connected_components(&active, small_width, small_height);
    let mut best: Option<ContentCandidate> = None;

    for component in components {
        let width = component.max_x - component.min_x + 1;
        let height = component.max_y - component.min_y + 1;
        let area = u64::from(width) * u64::from(height);
        let image_area = u64::from(small_width) * u64::from(small_height);
        let area_ratio = area as f32 / image_area as f32;
        let fill_ratio = component.pixel_count as f32 / area as f32;
        let aspect_ratio = width as f32 / height as f32;

        if !(0.12..=0.86).contains(&area_ratio)
            || fill_ratio < 0.72
            || !(0.35..=6.0).contains(&aspect_ratio)
        {
            continue;
        }

        let edge_margin_x = (small_width / 40).max(1);
        let edge_margin_y = (small_height / 40).max(1);
        if component.min_x <= edge_margin_x
            || component.min_y <= edge_margin_y
            || component.max_x + edge_margin_x >= small_width.saturating_sub(1)
            || component.max_y + edge_margin_y >= small_height.saturating_sub(1)
        {
            continue;
        }

        let center_x = (component.min_x + component.max_x + 1) as f32 / 2.0;
        let center_y = (component.min_y + component.max_y + 1) as f32 / 2.0;
        let normalized_dx = ((center_x / small_width as f32) - 0.5).abs() * 2.0;
        let normalized_dy = ((center_y / small_height as f32) - 0.5).abs() * 2.0;
        let centrality = 1.0
            - ((normalized_dx * normalized_dx + normalized_dy * normalized_dy).sqrt()
                / 2.0_f32.sqrt())
            .clamp(0.0, 1.0);
        let confidence = 0.42
            + fill_ratio.min(1.0) * 0.24
            + centrality * 0.18
            + (area_ratio / 0.60).min(1.0) * 0.16;

        let left = scale_floor(component.min_x, image.width(), small_width);
        let top = scale_floor(component.min_y, image.height(), small_height);
        let right = scale_ceil(component.max_x + 1, image.width(), small_width);
        let bottom = scale_ceil(component.max_y + 1, image.height(), small_height);
        if right <= left || bottom <= top {
            continue;
        }
        let candidate = ContentCandidate::new(
            PixelRect::new(left, top, right - left, bottom - top),
            confidence,
        );
        if best.is_none_or(|current| candidate.confidence > current.confidence) {
            best = Some(candidate);
        }
    }

    best
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

    if refined.area() * 2 >= rect.area() {
        refined
    } else {
        rect
    }
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
