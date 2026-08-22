from pathlib import Path

path = Path("src/capture/content_detector/visual.rs")
text = path.read_text(encoding="utf-8")

old_extend = '''fn extend_bottom_over_desktop_chrome(
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
    let mut last_structured = rows.end;
    let mut background_gap = 0u32;

    for y in rows.end..scan_end {
        let active_ratio = row_counts[y as usize] as f32 / column_width as f32;
        let mean_distance =
            analysis.mean_distance(PixelRect::new(columns.start, y, column_width, 1));
        let structured = active_ratio >= 0.02 || mean_distance >= 10.0;
        if structured {
            last_structured = y + 1;
            background_gap = 0;
        } else {
            background_gap += 1;
            if background_gap > 2 {
                break;
            }
        }
    }

    last_structured
}
'''

new_extend = '''fn extend_bottom_over_desktop_chrome(
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
'''

old_boundary = '''    let distinctive_ratio = distinctive as f32 / samples as f32;
    let boundary_strength = horizontal_boundary_strength(image, band.y, band.x, band.right());
    (0.003..=0.40).contains(&distinctive_ratio) && boundary_strength >= 4.0
'''

new_boundary = '''    let distinctive_ratio = distinctive as f32 / samples as f32;
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
'''

if old_extend not in text:
    raise SystemExit("extend_bottom_over_desktop_chrome block not found")
if old_boundary not in text:
    raise SystemExit("taskbar boundary block not found")

text = text.replace(old_extend, new_extend, 1)
text = text.replace(old_boundary, new_boundary, 1)
path.write_text(text, encoding="utf-8")
