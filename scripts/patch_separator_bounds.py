from pathlib import Path

path = Path("src/capture/content_detector/teams_frame.rs")
text = path.read_text(encoding="utf-8")

old = '''                        let start = search_start + start_offset as u32;
                        let end = start.saturating_add(total_width);
                        let Some((side, position_score)) = separator_side(rect, axis, start, end)
                        else {
                            continue;
                        };
'''
new = '''                        let start = search_start + start_offset as u32;
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
'''
if old not in text:
    raise SystemExit("separator candidate block not found")
text = text.replace(old, new, 1)

marker = '''fn band_match_ratio(
    image: &RgbaImage,
'''
insert = '''fn expand_separator_bounds(
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
        let match_ratio = band_match_ratio(image, rect, axis, position, palette.outer, 10)
            .max(band_match_ratio(
                image,
                rect,
                axis,
                position,
                palette.inner,
                10,
            ));
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
        let match_ratio = band_match_ratio(image, rect, axis, expanded_end, palette.outer, 10)
            .max(band_match_ratio(
                image,
                rect,
                axis,
                expanded_end,
                palette.inner,
                10,
            ));
        if match_ratio < 0.42 {
            break;
        }
        expanded_end += 1;
    }

    (expanded_start, expanded_end)
}

fn band_match_ratio(
    image: &RgbaImage,
'''
if marker not in text:
    raise SystemExit("band_match_ratio marker not found")
text = text.replace(marker, insert, 1)
path.write_text(text, encoding="utf-8")
