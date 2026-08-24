from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if new in text:
        return text
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{label}: expected one match, found {count}")
    return text.replace(old, new, 1)


path = Path("src/capture/content_detector/visual.rs")
text = path.read_text(encoding="utf-8")

text = replace_once(
    text,
    "    let rect = refine_projected_edges(image, rect);\n    let confidence = 0.50",
    "    let rect = refine_projected_edges(image, rect);\n    let rect = extend_top_over_continuous_surface_edges(image, rect);\n    let confidence = 0.50",
    "projection top extension call",
)

function = r'''fn extend_top_over_continuous_surface_edges(image: &RgbaImage, rect: PixelRect) -> PixelRect {
    if rect.y == 0
        || rect.y.saturating_mul(100) > image.height().saturating_mul(30)
        || rect.width.saturating_mul(100) < image.width().saturating_mul(35)
    {
        return rect;
    }

    let has_left_edge = rect.x > 0;
    let has_right_edge = rect.right() < image.width();
    let required_edges = u32::from(has_left_edge) + u32::from(has_right_edge);
    if required_edges == 0 {
        return rect;
    }

    let step = (rect.y / 180).max(1);
    let mut continuous_edges = 0u32;
    for boundary in [has_left_edge.then_some(rect.x), has_right_edge.then_some(rect.right())]
        .into_iter()
        .flatten()
    {
        let mut samples = 0u32;
        let mut matches = 0u32;
        let mut y = 0u32;
        while y < rect.y {
            samples += 1;
            if color_distance(
                image.get_pixel(boundary - 1, y).0,
                image.get_pixel(boundary, y).0,
            ) >= 3.0
            {
                matches += 1;
            }
            y = y.saturating_add(step);
        }

        if samples > 0 && matches.saturating_mul(100) >= samples.saturating_mul(68) {
            continuous_edges += 1;
        }
    }

    if continuous_edges == required_edges {
        PixelRect::new(rect.x, 0, rect.width, rect.bottom())
    } else {
        rect
    }
}

'''
marker = "fn refine_projected_edges(image: &RgbaImage, rect: PixelRect) -> PixelRect {"
if function not in text:
    if marker not in text:
        raise RuntimeError("top extension insertion marker not found")
    text = text.replace(marker, function + marker, 1)

path.write_text(text, encoding="utf-8")
