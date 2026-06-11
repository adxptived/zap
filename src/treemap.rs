//! Squarified-treemap layout and egui rendering for the GUI size view.
//!
//! The layout is iterative (no recursion), so arbitrarily many items can
//! never overflow the stack, and only the largest [`MAX_LAYOUT_ITEMS`]
//! entries are laid out — beyond that rectangles are sub-pixel anyway.

use std::path::{Path, PathBuf};

use eframe::egui;
use egui::{Color32, Pos2, Rect, Vec2};

#[derive(Debug, Clone)]
pub struct TreemapRect {
    pub rect: Rect,
    pub path: PathBuf,
    pub size: u64,
    pub color: Color32,
}

const MIN_RECT_SIZE_FOR_LABEL: f32 = 30.0;
const PADDING: f32 = 2.0;
const HEADER_HEIGHT: f32 = 20.0;

/// Hard cap on rectangles per layout. Rendering thousands of sub-pixel
/// rects every frame wastes CPU and conveys no information.
pub const MAX_LAYOUT_ITEMS: usize = 150;

/// Lay out `items` as a squarified treemap inside `bounds`.
/// Zero-sized items are skipped; items are sorted by size descending and
/// capped at [`MAX_LAYOUT_ITEMS`].
pub fn layout_treemap(items: &[(PathBuf, u64)], bounds: Rect) -> Vec<TreemapRect> {
    let mut non_zero: Vec<(PathBuf, u64)> =
        items.iter().filter(|(_, sz)| *sz > 0).cloned().collect();

    if non_zero.is_empty() || bounds.area() <= 0.0 {
        return vec![];
    }

    non_zero.sort_by_key(|item| std::cmp::Reverse(item.1));
    non_zero.truncate(MAX_LAYOUT_ITEMS);

    let total: u64 = non_zero.iter().map(|(_, sz)| *sz).sum();
    if total == 0 {
        return vec![];
    }

    squarify(&non_zero, bounds, total)
}

/// Iterative squarify: repeatedly grow a row along the short side of the
/// remaining bounds while the worst aspect ratio keeps improving, then
/// commit the row and shrink the bounds.
fn squarify(items: &[(PathBuf, u64)], bounds: Rect, total: u64) -> Vec<TreemapRect> {
    let scale = bounds.area() as f64 / total as f64; // px² per byte
    let mut out = Vec::with_capacity(items.len());
    let mut remaining = bounds;
    let mut i = 0;

    while i < items.len() {
        let short = remaining.width().min(remaining.height()) as f64;
        if short <= f64::EPSILON || remaining.area() <= 0.0 {
            break;
        }

        let mut row_end = i + 1;
        let mut row_area = items[i].1 as f64 * scale;
        let mut best = worst_ratio(&items[i..row_end], scale, row_area, short);

        while row_end < items.len() {
            let next_area = row_area + items[row_end].1 as f64 * scale;
            let candidate = worst_ratio(&items[i..=row_end], scale, next_area, short);
            if candidate <= best {
                row_end += 1;
                row_area = next_area;
                best = candidate;
            } else {
                break;
            }
        }

        remaining = place_row(&items[i..row_end], scale, row_area, remaining, &mut out);
        i = row_end;
    }

    out
}

/// Worst (largest) aspect ratio in a row of thickness `row_area / short`.
fn worst_ratio(row: &[(PathBuf, u64)], scale: f64, row_area: f64, short: f64) -> f64 {
    let thickness = row_area / short;
    if thickness <= f64::EPSILON {
        return f64::MAX;
    }
    row.iter()
        .map(|(_, sz)| {
            let len = (*sz as f64 * scale) / thickness;
            if len <= f64::EPSILON {
                f64::MAX
            } else {
                (thickness / len).max(len / thickness)
            }
        })
        .fold(0.0, f64::max)
}

/// Lay a committed row along the short side of `bounds` and return the
/// remaining bounds.
fn place_row(
    row: &[(PathBuf, u64)],
    scale: f64,
    row_area: f64,
    bounds: Rect,
    out: &mut Vec<TreemapRect>,
) -> Rect {
    if bounds.width() >= bounds.height() {
        // Vertical strip on the left edge.
        let w = (row_area / bounds.height() as f64) as f32;
        let mut y = bounds.min.y;
        for (path, sz) in row {
            let h = ((*sz as f64 * scale) / w.max(f32::EPSILON) as f64) as f32;
            let rect =
                Rect::from_min_size(Pos2::new(bounds.min.x, y), Vec2::new(w, h)).intersect(bounds);
            push_rect(out, rect, path, *sz);
            y += h;
        }
        Rect::from_min_max(Pos2::new(bounds.min.x + w, bounds.min.y), bounds.max)
    } else {
        // Horizontal strip on the top edge.
        let h = (row_area / bounds.width() as f64) as f32;
        let mut x = bounds.min.x;
        for (path, sz) in row {
            let w = ((*sz as f64 * scale) / h.max(f32::EPSILON) as f64) as f32;
            let rect =
                Rect::from_min_size(Pos2::new(x, bounds.min.y), Vec2::new(w, h)).intersect(bounds);
            push_rect(out, rect, path, *sz);
            x += w;
        }
        Rect::from_min_max(Pos2::new(bounds.min.x, bounds.min.y + h), bounds.max)
    }
}

fn push_rect(out: &mut Vec<TreemapRect>, rect: Rect, path: &Path, size: u64) {
    let index = out.len();
    out.push(TreemapRect {
        rect,
        path: path.to_path_buf(),
        size,
        color: item_color(index),
    });
}

/// Stable per-item color from a small desaturated palette so neighbouring
/// rectangles are distinguishable.
fn item_color(index: usize) -> Color32 {
    const PALETTE: [Color32; 6] = [
        Color32::from_rgb(70, 90, 120),
        Color32::from_rgb(85, 110, 95),
        Color32::from_rgb(115, 90, 80),
        Color32::from_rgb(95, 80, 115),
        Color32::from_rgb(110, 105, 75),
        Color32::from_rgb(75, 105, 115),
    ];
    PALETTE[index % PALETTE.len()]
}

/// Render a treemap of `items` (path, size) pairs. `total_size` is shown in
/// the header and may include entries beyond the layout cap.
pub fn treemap_ui(ui: &mut egui::Ui, items: &[(PathBuf, u64)], total_size: u64) -> egui::Response {
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

        if !items.is_empty() {
            let laid_out = layout_treemap(items, content_rect);

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
        assert_eq!(rects.len(), 10);
        for r in &rects {
            assert!(
                bounds.expand(0.5).contains_rect(r.rect),
                "rect {:?} outside bounds {:?}",
                r.rect,
                bounds
            );
        }
    }

    #[test]
    fn test_treemap_rects_do_not_overlap() {
        let items: Vec<(PathBuf, u64)> = (0..12)
            .map(|i| (PathBuf::from(format!("item{i}")), (i as u64 + 1) * 37))
            .collect();
        let bounds = Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 250.0));
        let rects = layout_treemap(&items, bounds);
        for (a_idx, a) in rects.iter().enumerate() {
            for b in rects.iter().skip(a_idx + 1) {
                // Note: Rect::intersect on disjoint rects yields negative
                // extents, so clamp each axis to zero before multiplying.
                let w = (a.rect.max.x.min(b.rect.max.x) - a.rect.min.x.max(b.rect.min.x)).max(0.0);
                let h = (a.rect.max.y.min(b.rect.max.y) - a.rect.min.y.max(b.rect.min.y)).max(0.0);
                assert!(
                    w * h <= 1.0,
                    "rects overlap by {}px²: {:?} vs {:?}",
                    w * h,
                    a.rect,
                    b.rect
                );
            }
        }
    }

    #[test]
    fn test_treemap_area_proportional_to_size() {
        let items = vec![(PathBuf::from("big"), 300), (PathBuf::from("small"), 100)];
        let bounds = Rect::from_min_size(Pos2::ZERO, Vec2::new(200.0, 200.0));
        let rects = layout_treemap(&items, bounds);
        let big = rects.iter().find(|r| r.path.ends_with("big")).unwrap();
        let small = rects.iter().find(|r| r.path.ends_with("small")).unwrap();
        let ratio = big.rect.area() / small.rect.area();
        assert!(
            (ratio - 3.0).abs() < 0.2,
            "expected ~3x area ratio, got {ratio}"
        );
    }

    #[test]
    fn test_treemap_huge_item_count_does_not_overflow_stack_and_is_capped() {
        // The previous recursive implementation overflowed the stack on
        // large inputs; this must complete and respect MAX_LAYOUT_ITEMS.
        let items = make_items(50_000, 10);
        let bounds = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        let rects = layout_treemap(&items, bounds);
        assert_eq!(rects.len(), MAX_LAYOUT_ITEMS);
    }

    #[test]
    fn test_treemap_keeps_largest_items_when_capped() {
        let mut items = make_items(MAX_LAYOUT_ITEMS + 5, 1);
        items.push((PathBuf::from("huge"), 1_000_000));
        let bounds = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        let rects = layout_treemap(&items, bounds);
        assert_eq!(rects.len(), MAX_LAYOUT_ITEMS);
        assert!(rects.iter().any(|r| r.path.ends_with("huge")));
    }
}
