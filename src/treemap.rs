use std::path::PathBuf;

use eframe::egui;
use egui::{Color32, Pos2, Rect, Vec2};

#[derive(Debug, Clone)]
pub struct TreemapRect {
    pub rect: Rect,
    pub path: PathBuf,
    pub size: u64,
    pub color: Color32,
    pub depth: usize,
}

const MIN_RECT_SIZE_FOR_LABEL: f32 = 30.0;
const PADDING: f32 = 2.0;
const HEADER_HEIGHT: f32 = 20.0;

pub fn layout_treemap(items: &[(PathBuf, u64)], bounds: Rect) -> Vec<TreemapRect> {
    let mut non_zero: Vec<(PathBuf, u64)> =
        items.iter().filter(|(_, sz)| *sz > 0).cloned().collect();

    if non_zero.is_empty() {
        return vec![];
    }

    non_zero.sort_by(|a, b| b.1.cmp(&a.1));

    let total: u64 = non_zero.iter().map(|(_, sz)| *sz).sum();
    if total == 0 {
        return vec![];
    }

    let mut result = Vec::with_capacity(non_zero.len());
    squarify(&mut non_zero, &[], bounds, total, &mut result);
    result
}

fn squarify(
    remaining: &mut Vec<(PathBuf, u64)>,
    row: &[(PathBuf, u64)],
    bounds: Rect,
    total: u64,
    out: &mut Vec<TreemapRect>,
) {
    if remaining.is_empty() {
        layout_row(row, bounds, total, out, 0);
        return;
    }

    if row.is_empty() {
        let next = remaining.remove(0);
        squarify(remaining, &[next], bounds, total, out);
        return;
    }

    let next = remaining[0].clone();
    let mut current_row: Vec<(PathBuf, u64)> = row.to_vec();
    current_row.push(next);

    if worst_ratio(&current_row, bounds) <= worst_ratio(row, bounds) {
        remaining.remove(0);
        squarify(remaining, &current_row, bounds, total, out);
    } else {
        layout_row(row, bounds, total, out, 0);
        let row_total: u64 = row.iter().map(|(_, s)| s).sum();
        let row_area = (row_total as f64 / total as f64) * bounds.area() as f64;

        if bounds.width() >= bounds.height() {
            let row_w = (row_area / bounds.height() as f64) as f32;
            let remaining_bounds =
                Rect::from_min_max(Pos2::new(bounds.min.x + row_w, bounds.min.y), bounds.max);
            squarify(remaining, &[], remaining_bounds, total, out);
        } else {
            let row_h = (row_area / bounds.width() as f64) as f32;
            let remaining_bounds =
                Rect::from_min_max(Pos2::new(bounds.min.x, bounds.min.y + row_h), bounds.max);
            squarify(remaining, &[], remaining_bounds, total, out);
        }
    }
}

fn layout_row(
    row: &[(PathBuf, u64)],
    bounds: Rect,
    total: u64,
    out: &mut Vec<TreemapRect>,
    base_depth: usize,
) {
    if row.is_empty() {
        return;
    }

    let row_total: u64 = row.iter().map(|(_, s)| s).sum();
    let row_area = (row_total as f64 / total as f64) * bounds.area() as f64;
    let is_horizontal = bounds.width() >= bounds.height();

    if is_horizontal {
        let row_h = (row_area / bounds.width() as f64) as f32;
        let mut offset = 0.0f32;
        for (path, sz) in row {
            let w = (*sz as f64 / row_total as f64) * bounds.width() as f64;
            let rect = Rect::from_min_size(
                Pos2::new(bounds.min.x + offset, bounds.min.y),
                Vec2::new(w as f32, row_h),
            );
            out.push(TreemapRect {
                rect,
                path: path.clone(),
                size: *sz,
                color: depth_color(base_depth),
                depth: base_depth,
            });
            offset += w as f32;
        }
    } else {
        let row_w = (row_area / bounds.height() as f64) as f32;
        let mut offset = 0.0f32;
        for (path, sz) in row {
            let h = (*sz as f64 / row_total as f64) * bounds.height() as f64;
            let rect = Rect::from_min_size(
                Pos2::new(bounds.min.x, bounds.min.y + offset),
                Vec2::new(row_w, h as f32),
            );
            out.push(TreemapRect {
                rect,
                path: path.clone(),
                size: *sz,
                color: depth_color(base_depth),
                depth: base_depth,
            });
            offset += h as f32;
        }
    }
}

fn worst_ratio(row: &[(PathBuf, u64)], bounds: Rect) -> f64 {
    if row.is_empty() {
        return f64::MAX;
    }
    let sum: u64 = row.iter().map(|(_, s)| s).sum();
    let min_sz = row.iter().map(|(_, s)| s).min().copied().unwrap_or(1);
    let max_sz = row.iter().map(|(_, s)| s).max().copied().unwrap_or(1);
    let area = bounds.area() as f64;
    let s = sum as f64;
    let short = if bounds.width() >= bounds.height() {
        bounds.height() as f64
    } else {
        bounds.width() as f64
    };
    let w = short / s * area / s;
    (w * w * max_sz as f64 / area).max(area / (w * w * min_sz as f64))
}

fn depth_color(depth: usize) -> Color32 {
    let base: u8 = 60u8.saturating_add((depth as u32 * 25).min(180) as u8);
    Color32::from_rgb(base, base.saturating_add(20), base.saturating_add(50))
}

/// Render a treemap UI widget. Returns a rect for hover/click interaction.
pub fn treemap_ui(ui: &mut egui::Ui, rects: &[TreemapRect], total_size: u64) -> egui::Response {
    let desired_size = ui.available_size();
    let (id, rect) = ui.allocate_space(desired_size);

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();

        let header = Rect::from_min_size(rect.min, Vec2::new(rect.width(), HEADER_HEIGHT));
        painter.rect_filled(header, 0.0, Color32::from_rgb(35, 38, 45));
        painter.text(
            header.center(),
            egui::Align2::CENTER_CENTER,
            format!("Disk usage — {}", crate::size::format_size(total_size)),
            egui::FontId::new(12.0, egui::FontFamily::Proportional),
            Color32::from_rgb(200, 204, 210),
        );

        let content_rect = Rect::from_min_max(
            Pos2::new(rect.min.x, rect.min.y + HEADER_HEIGHT + 4.0),
            rect.max,
        );

        if !rects.is_empty() {
            let layout_items: Vec<(PathBuf, u64)> =
                rects.iter().map(|r| (r.path.clone(), r.size)).collect();
            let laid_out = layout_treemap(&layout_items, content_rect);

            for item in &laid_out {
                let inner = item.rect.shrink(PADDING);
                if inner.area() <= 0.0 {
                    continue;
                }
                painter.rect_filled(inner, 3.0, item.color);
                painter.rect_stroke(
                    inner,
                    3.0,
                    egui::Stroke::new(1.0, Color32::from_rgb(20, 22, 28)),
                    egui::StrokeKind::Inside,
                );
                if inner.width() > MIN_RECT_SIZE_FOR_LABEL
                    && inner.height() > MIN_RECT_SIZE_FOR_LABEL
                {
                    let name = item
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| item.path.to_string_lossy().to_string());
                    let label = format!("{}\n{}", name, crate::size::format_size(item.size));
                    painter.text(
                        inner.center(),
                        egui::Align2::CENTER_CENTER,
                        label,
                        egui::FontId::new(10.0, egui::FontFamily::Proportional),
                        Color32::WHITE,
                    );
                }
            }
        } else {
            painter.text(
                content_rect.center(),
                egui::Align2::CENTER_CENTER,
                "Collecting size data...",
                egui::FontId::new(12.0, egui::FontFamily::Proportional),
                Color32::from_rgb(150, 154, 164),
            );
        }
    }

    ui.interact(rect, id, egui::Sense::click())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_items(count: usize, size_each: u64) -> Vec<(PathBuf, u64)> {
        (0..count)
            .map(|i| (PathBuf::from(format!("item{i}")), size_each))
            .collect()
    }

    #[test]
    fn test_treemap_single_rect_fills_canvas() {
        let items = vec![(PathBuf::from("a"), 100u64)];
        let rects = layout_treemap(
            &items,
            Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 100.0)),
        );
        assert_eq!(rects.len(), 1);
        let r = rects[0].rect;
        assert!((r.width() - 100.0).abs() < 1.0);
        assert!((r.height() - 100.0).abs() < 1.0);
    }

    #[test]
    fn test_treemap_two_equal_items_split() {
        let items = vec![(PathBuf::from("a"), 50), (PathBuf::from("b"), 50)];
        let rects = layout_treemap(
            &items,
            Rect::from_min_size(Pos2::ZERO, Vec2::new(200.0, 100.0)),
        );
        assert_eq!(rects.len(), 2);
        for r in &rects {
            assert!(r.rect.area() > 0.0);
        }
    }

    #[test]
    fn test_treemap_zero_size_items_skipped() {
        let items = vec![(PathBuf::from("a"), 0), (PathBuf::from("b"), 100)];
        let rects = layout_treemap(
            &items,
            Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 100.0)),
        );
        assert_eq!(rects.len(), 1);
    }

    #[test]
    fn test_treemap_empty_input_returns_empty() {
        let items: Vec<(PathBuf, u64)> = vec![];
        let rects = layout_treemap(
            &items,
            Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 100.0)),
        );
        assert!(rects.is_empty());
    }

    #[test]
    fn test_treemap_all_rects_within_bounds() {
        let items = make_items(10, 100);
        let bounds = Rect::from_min_size(Pos2::ZERO, Vec2::new(300.0, 200.0));
        let rects = layout_treemap(&items, bounds);
        for r in &rects {
            assert!(
                bounds.contains_rect(r.rect),
                "rect {:?} outside bounds {:?}",
                r.rect,
                bounds
            );
        }
    }
}
