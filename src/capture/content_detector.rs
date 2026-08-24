mod teams_frame;
mod visual;

use image::RgbaImage;

use self::{
    teams_frame::{detect_teams_candidate, refine_teams_rect},
    visual::{detect_visual_candidate, refine_uniform_margins},
};

const SEMANTIC_CONFIDENCE_THRESHOLD: f32 = 0.55;
const VISUAL_CONFIDENCE_THRESHOLD: f32 = 0.84;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PixelRect {
    pub(crate) const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn right(self) -> u32 {
        self.x.saturating_add(self.width)
    }

    fn bottom(self) -> u32 {
        self.y.saturating_add(self.height)
    }

    fn area(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    fn intersection(self, other: Self) -> Option<Self> {
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());

        (right > left && bottom > top).then_some(Self::new(left, top, right - left, bottom - top))
    }

    fn overlap_over_smaller(self, other: Self) -> f32 {
        let Some(intersection) = self.intersection(other) else {
            return 0.0;
        };
        let smaller = self.area().min(other.area());
        if smaller == 0 {
            0.0
        } else {
            intersection.area() as f32 / smaller as f32
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ContentCandidate {
    pub rect: PixelRect,
    pub confidence: f32,
    is_exclusion: bool,
}

impl ContentCandidate {
    pub(crate) fn new(rect: PixelRect, confidence: f32) -> Self {
        Self {
            rect,
            confidence: confidence.clamp(0.0, 1.0),
            is_exclusion: false,
        }
    }

    pub(crate) fn exclusion(rect: PixelRect) -> Self {
        Self {
            rect,
            confidence: 1.0,
            is_exclusion: true,
        }
    }
}

pub(crate) fn select_content_rect(
    image: &RgbaImage,
    semantic_candidates: &[ContentCandidate],
    allow_visual_fallback: bool,
) -> Option<PixelRect> {
    let visual_candidate = choose_visual_candidate(
        detect_visual_candidate(image),
        detect_teams_candidate(image, PixelRect::new(0, 0, image.width(), image.height())),
    );
    let exclusions: Vec<PixelRect> = semantic_candidates
        .iter()
        .filter(|candidate| candidate.is_exclusion)
        .map(|candidate| candidate.rect)
        .collect();
    let mut best: Option<(f32, PixelRect)> = None;

    for candidate in semantic_candidates
        .iter()
        .filter(|candidate| !candidate.is_exclusion)
    {
        if candidate.confidence < SEMANTIC_CONFIDENCE_THRESHOLD
            || !is_plausible_rect(candidate.rect, image.width(), image.height())
        {
            continue;
        }

        let mut score = candidate.confidence + specificity_bonus(candidate.rect, image);
        for other in semantic_candidates
            .iter()
            .filter(|other| !other.is_exclusion)
        {
            if candidate.rect == other.rect {
                continue;
            }
            let overlap = candidate.rect.overlap_over_smaller(other.rect);
            if overlap >= 0.82 {
                score += other.confidence.min(0.12) * overlap;
            }
        }
        if let Some(visual) = visual_candidate {
            let overlap = candidate.rect.overlap_over_smaller(visual.rect);
            if overlap >= 0.70 {
                score += 0.12 * visual.confidence * overlap;
            }
        }

        let Some(refined) = finalize_rect(image, candidate.rect, &exclusions) else {
            continue;
        };
        if best.is_none_or(|(best_score, _)| score > best_score) {
            best = Some((score, refined));
        }
    }

    if let Some((_, rect)) = best {
        return Some(rect);
    }

    if !allow_visual_fallback {
        return None;
    }

    visual_candidate
        .filter(|candidate| candidate.confidence >= VISUAL_CONFIDENCE_THRESHOLD)
        .and_then(|candidate| finalize_rect(image, candidate.rect, &exclusions))
}

fn choose_visual_candidate(
    visual: Option<ContentCandidate>,
    teams: Option<ContentCandidate>,
) -> Option<ContentCandidate> {
    match (visual, teams) {
        (Some(visual), Some(teams)) => {
            let overlap = visual.rect.overlap_over_smaller(teams.rect);
            let teams_is_more_specific = teams.rect.area() * 100 <= visual.rect.area() * 97;
            if teams.confidence >= visual.confidence || (overlap >= 0.72 && teams_is_more_specific)
            {
                Some(teams)
            } else {
                Some(visual)
            }
        }
        (Some(candidate), None) | (None, Some(candidate)) => Some(candidate),
        (None, None) => None,
    }
}

fn finalize_rect(
    image: &RgbaImage,
    rect: PixelRect,
    exclusions: &[PixelRect],
) -> Option<PixelRect> {
    let teams_refined = refine_teams_rect(image, rect);
    let refined = refine_uniform_margins(image, teams_refined);
    let trimmed = trim_excluded_edge_regions(refined, exclusions);
    let teams_refined = refine_teams_rect(image, trimmed);
    let refined = refine_uniform_margins(image, teams_refined);
    is_plausible_rect(refined, image.width(), image.height()).then_some(refined)
}

fn trim_excluded_edge_regions(rect: PixelRect, exclusions: &[PixelRect]) -> PixelRect {
    if exclusions.is_empty() || rect.width == 0 || rect.height == 0 {
        return rect;
    }

    let tolerance_x = (rect.width / 40).max(4);
    let tolerance_y = (rect.height / 40).max(4);
    let mut left_cut = rect.x;
    let mut right_cut = rect.right();
    let mut top_cut = rect.y;
    let mut bottom_cut = rect.bottom();
    let mut left_intervals = Vec::new();
    let mut right_intervals = Vec::new();
    let mut top_intervals = Vec::new();
    let mut bottom_intervals = Vec::new();

    for exclusion in exclusions {
        let Some(overlap) = rect.intersection(*exclusion) else {
            continue;
        };
        let width_ratio = overlap.width as f32 / rect.width as f32;
        let height_ratio = overlap.height as f32 / rect.height as f32;

        if (0.04..=0.45).contains(&width_ratio) {
            if overlap.x <= rect.x.saturating_add(tolerance_x) {
                left_cut = left_cut.max(overlap.right());
                left_intervals.push((overlap.y, overlap.bottom()));
            }
            if overlap.right().saturating_add(tolerance_x) >= rect.right() {
                right_cut = right_cut.min(overlap.x);
                right_intervals.push((overlap.y, overlap.bottom()));
            }
        }

        if (0.04..=0.35).contains(&height_ratio) {
            if overlap.y <= rect.y.saturating_add(tolerance_y) {
                top_cut = top_cut.max(overlap.bottom());
                top_intervals.push((overlap.x, overlap.right()));
            }
            if overlap.bottom().saturating_add(tolerance_y) >= rect.bottom() {
                bottom_cut = bottom_cut.min(overlap.y);
                bottom_intervals.push((overlap.x, overlap.right()));
            }
        }
    }

    if covered_length(left_intervals) * 100 < rect.height * 45 {
        left_cut = rect.x;
    }
    if covered_length(right_intervals) * 100 < rect.height * 45 {
        right_cut = rect.right();
    }
    if covered_length(top_intervals) * 100 < rect.width * 45 {
        top_cut = rect.y;
    }
    if covered_length(bottom_intervals) * 100 < rect.width * 45 {
        bottom_cut = rect.bottom();
    }

    if right_cut <= left_cut || bottom_cut <= top_cut {
        return rect;
    }

    let trimmed = PixelRect::new(
        left_cut,
        top_cut,
        right_cut - left_cut,
        bottom_cut - top_cut,
    );
    let enough_width = trimmed.width * 100 >= rect.width * 45;
    let enough_height = trimmed.height * 100 >= rect.height * 45;
    let enough_area = trimmed.area() * 100 >= rect.area() * 50;
    if enough_width && enough_height && enough_area {
        trimmed
    } else {
        rect
    }
}

fn covered_length(mut intervals: Vec<(u32, u32)>) -> u32 {
    if intervals.is_empty() {
        return 0;
    }
    intervals.sort_unstable_by_key(|interval| interval.0);
    let mut total = 0u32;
    let mut current = intervals[0];
    for interval in intervals.into_iter().skip(1) {
        if interval.0 <= current.1 {
            current.1 = current.1.max(interval.1);
        } else {
            total = total.saturating_add(current.1.saturating_sub(current.0));
            current = interval;
        }
    }
    total.saturating_add(current.1.saturating_sub(current.0))
}

fn specificity_bonus(rect: PixelRect, image: &RgbaImage) -> f32 {
    let image_area = u64::from(image.width()) * u64::from(image.height());
    if image_area == 0 {
        return 0.0;
    }
    let area_ratio = rect.area() as f32 / image_area as f32;
    if (0.16..=0.82).contains(&area_ratio) {
        0.08 * (1.0 - ((area_ratio - 0.48).abs() / 0.48).min(1.0))
    } else if area_ratio > 0.96 {
        -0.10
    } else {
        0.0
    }
}

fn is_plausible_rect(rect: PixelRect, image_width: u32, image_height: u32) -> bool {
    if image_width == 0 || image_height == 0 || rect.width == 0 || rect.height == 0 {
        return false;
    }
    if rect.right() > image_width || rect.bottom() > image_height {
        return false;
    }

    let min_width = (image_width / 7).max(96).min(image_width);
    let min_height = (image_height / 7).max(54).min(image_height);
    if rect.width < min_width || rect.height < min_height {
        return false;
    }

    let image_area = u64::from(image_width) * u64::from(image_height);
    let area_ratio = rect.area() as f32 / image_area as f32;
    let aspect_ratio = rect.width as f32 / rect.height as f32;
    (0.07..=0.995).contains(&area_ratio) && (0.30..=6.5).contains(&aspect_ratio)
}

#[cfg(test)]
mod tests {
    use image::{Rgba, RgbaImage};

    use super::{ContentCandidate, PixelRect, select_content_rect};

    #[test]
    fn semantic_candidate_is_used_for_dark_content() {
        let image = RgbaImage::from_pixel(1280, 720, Rgba([18, 18, 20, 255]));
        let expected = PixelRect::new(120, 80, 960, 540);
        let detected = select_content_rect(&image, &[ContentCandidate::new(expected, 0.92)], false);

        assert_eq!(detected, Some(expected));
    }

    #[test]
    fn participants_panel_is_removed_from_semantic_stage() {
        let image = RgbaImage::from_pixel(1600, 900, Rgba([24, 24, 28, 255]));
        let stage = PixelRect::new(100, 70, 1400, 760);
        let participants = PixelRect::new(1220, 80, 280, 740);
        let detected = select_content_rect(
            &image,
            &[
                ContentCandidate::new(stage, 0.91),
                ContentCandidate::exclusion(participants),
            ],
            false,
        )
        .expect("shared content should remain");

        assert_eq!(detected, PixelRect::new(100, 70, 1120, 760));
    }

    #[test]
    fn stacked_participant_tiles_form_one_excluded_side_band() {
        let image = RgbaImage::from_pixel(1600, 900, Rgba([24, 24, 28, 255]));
        let stage = PixelRect::new(100, 70, 1400, 760);
        let detected = select_content_rect(
            &image,
            &[
                ContentCandidate::new(stage, 0.91),
                ContentCandidate::exclusion(PixelRect::new(1240, 90, 260, 220)),
                ContentCandidate::exclusion(PixelRect::new(1240, 320, 260, 220)),
                ContentCandidate::exclusion(PixelRect::new(1240, 550, 260, 240)),
            ],
            false,
        )
        .expect("shared content should remain");

        assert_eq!(detected, PixelRect::new(100, 70, 1140, 760));
    }

    #[test]
    fn isolated_negative_button_does_not_crop_the_stage() {
        let image = RgbaImage::from_pixel(1600, 900, Rgba([24, 24, 28, 255]));
        let stage = PixelRect::new(100, 70, 1400, 760);
        let detected = select_content_rect(
            &image,
            &[
                ContentCandidate::new(stage, 0.91),
                ContentCandidate::exclusion(PixelRect::new(1420, 760, 80, 70)),
            ],
            false,
        );

        assert_eq!(detected, Some(stage));
    }

    #[test]
    fn visual_detection_scales_with_window_resolution() {
        for (width, height) in [(800, 600), (1920, 1080), (480, 320)] {
            let expected = PixelRect::new(
                width * 8 / 100,
                height * 12 / 100,
                width * 72 / 100,
                height * 72 / 100,
            );
            let image = mock_teams_window(width, height, expected);
            let detected =
                select_content_rect(&image, &[], true).expect("content should be detected");
            let intersection = detected
                .intersection(expected)
                .expect("detected content should overlap the expected region");
            let union = detected.area() + expected.area() - intersection.area();
            let intersection_over_union = intersection.area() as f32 / union as f32;

            assert!(
                intersection_over_union >= 0.86,
                "detected={detected:?}, expected={expected:?}, iou={intersection_over_union}"
            );
        }
    }

    #[test]
    fn ambiguous_uniform_frame_fails_closed() {
        let image = RgbaImage::from_pixel(1280, 720, Rgba([24, 24, 28, 255]));

        assert_eq!(select_content_rect(&image, &[], true), None);
    }

    #[test]
    fn semantic_rect_trims_matching_letterbox_bands() {
        let mut image = RgbaImage::from_pixel(1000, 700, Rgba([24, 24, 28, 255]));
        fill_rect(
            &mut image,
            PixelRect::new(100, 100, 800, 500),
            Rgba([230, 230, 232, 255]),
        );
        fill_rect(
            &mut image,
            PixelRect::new(100, 100, 800, 40),
            Rgba([24, 24, 28, 255]),
        );
        fill_rect(
            &mut image,
            PixelRect::new(100, 560, 800, 40),
            Rgba([24, 24, 28, 255]),
        );

        let detected = select_content_rect(
            &image,
            &[ContentCandidate::new(
                PixelRect::new(100, 100, 800, 500),
                0.95,
            )],
            false,
        )
        .expect("semantic content should be detected");

        assert_eq!(detected, PixelRect::new(100, 140, 800, 420));
    }

    #[test]
    fn strict_mode_rejects_visual_only_detection() {
        let expected = PixelRect::new(80, 72, 720, 432);
        let image = mock_teams_window(1000, 600, expected);

        assert_eq!(select_content_rect(&image, &[], false), None);
    }

    fn mock_teams_window(width: u32, height: u32, content: PixelRect) -> RgbaImage {
        let mut image = RgbaImage::from_pixel(width, height, Rgba([28, 28, 32, 255]));
        fill_rect(
            &mut image,
            PixelRect::new(0, 0, width, height * 7 / 100),
            Rgba([35, 35, 39, 255]),
        );
        fill_rect(
            &mut image,
            PixelRect::new(0, height * 91 / 100, width, height * 9 / 100),
            Rgba([33, 33, 37, 255]),
        );
        fill_rect(&mut image, content, Rgba([235, 235, 238, 255]));

        let side_x = width * 82 / 100;
        fill_rect(
            &mut image,
            PixelRect::new(side_x, height / 10, width * 15 / 100, height * 78 / 100),
            Rgba([66, 66, 72, 255]),
        );
        let marker_width = (content.width / 10).max(4);
        let marker_height = (content.height / 18).max(3);
        for row in 0..3 {
            for column in 0..4 {
                let x = content.x + content.width * (column * 2 + 1) / 10;
                let y = content.y + content.height * (row * 2 + 1) / 7;
                fill_rect(
                    &mut image,
                    PixelRect::new(x, y, marker_width, marker_height),
                    Rgba([80 + column as u8 * 20, 110 + row as u8 * 20, 160, 255]),
                );
            }
        }
        image
    }

    fn fill_rect(image: &mut RgbaImage, rect: PixelRect, color: Rgba<u8>) {
        let right = rect.x.saturating_add(rect.width).min(image.width());
        let bottom = rect.y.saturating_add(rect.height).min(image.height());
        for y in rect.y..bottom {
            for x in rect.x..right {
                image.put_pixel(x, y, color);
            }
        }
    }
}
