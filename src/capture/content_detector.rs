mod visual;

use image::RgbaImage;

use self::visual::{detect_visual_candidate, refine_uniform_margins};

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
}

impl ContentCandidate {
    pub(crate) fn new(rect: PixelRect, confidence: f32) -> Self {
        Self {
            rect,
            confidence: confidence.clamp(0.0, 1.0),
        }
    }
}

pub(crate) fn select_content_rect(
    image: &RgbaImage,
    semantic_candidates: &[ContentCandidate],
    allow_visual_fallback: bool,
) -> Option<PixelRect> {
    let visual_candidate = detect_visual_candidate(image);
    let mut best: Option<(f32, PixelRect)> = None;

    for candidate in semantic_candidates {
        if candidate.confidence < SEMANTIC_CONFIDENCE_THRESHOLD
            || !is_plausible_rect(candidate.rect, image.width(), image.height())
        {
            continue;
        }

        let mut score = candidate.confidence + specificity_bonus(candidate.rect, image);
        for other in semantic_candidates {
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

        let refined = refine_uniform_margins(image, candidate.rect);
        if !is_plausible_rect(refined, image.width(), image.height()) {
            continue;
        }

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
        .map(|candidate| refine_uniform_margins(image, candidate.rect))
        .filter(|rect| is_plausible_rect(*rect, image.width(), image.height()))
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
            let tolerance_x = (width / 80).max(4);
            let tolerance_y = (height / 80).max(4);

            assert!(detected.x.abs_diff(expected.x) <= tolerance_x);
            assert!(detected.y.abs_diff(expected.y) <= tolerance_y);
            assert!(detected.width.abs_diff(expected.width) <= tolerance_x * 2);
            assert!(detected.height.abs_diff(expected.height) <= tolerance_y * 2);
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
