//! Continuous vertical page layout, in PDF points.

use std::ops::Range;

use pdf_engine::PageSize;

/// Gap between two pages, in points.
pub const PAGE_GAP_PT: f32 = 10.0;

/// Space above the first page and below the last page, in points.
pub const MARGIN_PT: f32 = 10.0;

/// Where one page sits in the document, in points. `y` grows downward.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageSlot {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl PageSlot {
    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }
}

/// Pages stacked top to bottom, each centered on the widest page.
#[derive(Clone, Debug)]
pub struct DocLayout {
    slots: Vec<PageSlot>,
    width: f32,
    height: f32,
}

impl DocLayout {
    pub fn new(pages: &[PageSize]) -> Self {
        let width = pages.iter().map(|p| p.width_pt).fold(0.0, f32::max);
        let mut y = MARGIN_PT;
        let slots = pages
            .iter()
            .map(|page| {
                let slot = PageSlot {
                    x: (width - page.width_pt) / 2.0,
                    y,
                    width: page.width_pt,
                    height: page.height_pt,
                };
                y += page.height_pt + PAGE_GAP_PT;
                slot
            })
            .collect();
        let height = if pages.is_empty() {
            0.0
        } else {
            y - PAGE_GAP_PT + MARGIN_PT
        };
        DocLayout {
            slots,
            width,
            height,
        }
    }

    pub fn slots(&self) -> &[PageSlot] {
        &self.slots
    }

    pub fn width(&self) -> f32 {
        self.width
    }

    pub fn height(&self) -> f32 {
        self.height
    }

    /// Indices of the pages that overlap the vertical range `top..bottom`.
    pub fn visible(&self, top: f32, bottom: f32) -> Range<usize> {
        let start = self.slots.partition_point(|s| s.bottom() <= top);
        let end = self.slots.partition_point(|s| s.y < bottom);
        start..end.max(start)
    }

    /// The page at vertical position `y`; in a gap between pages, the nearer page.
    pub fn page_at(&self, y: f32) -> usize {
        self.slots
            .partition_point(|s| s.bottom() + PAGE_GAP_PT / 2.0 < y)
            .min(self.slots.len().saturating_sub(1))
    }

    /// Scroll position that shows page `index` at the top of the view.
    pub fn page_top(&self, index: usize) -> f32 {
        self.slots
            .get(index)
            .map_or(0.0, |s| (s.y - MARGIN_PT).max(0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(width_pt: f32, height_pt: f32) -> PageSize {
        PageSize {
            width_pt,
            height_pt,
        }
    }

    fn three_pages() -> DocLayout {
        // A4 portrait, A4 landscape, small.
        DocLayout::new(&[size(595.0, 842.0), size(842.0, 595.0), size(300.0, 200.0)])
    }

    #[test]
    fn stacks_pages_with_gaps_and_margins() {
        let layout = three_pages();
        let ys: Vec<f32> = layout.slots().iter().map(|s| s.y).collect();
        assert_eq!(ys, vec![10.0, 862.0, 1467.0]);
        assert_eq!(layout.height(), 1467.0 + 200.0 + MARGIN_PT);
        assert_eq!(layout.width(), 842.0);
    }

    #[test]
    fn centers_narrow_pages_on_the_widest() {
        let layout = three_pages();
        let xs: Vec<f32> = layout.slots().iter().map(|s| s.x).collect();
        assert_eq!(xs, vec![123.5, 0.0, 271.0]);
    }

    #[test]
    fn finds_visible_pages() {
        let layout = three_pages();
        assert_eq!(layout.visible(0.0, 500.0), 0..1);
        assert_eq!(layout.visible(800.0, 900.0), 0..2);
        // Entirely inside the gap between page 1 and page 2.
        assert_eq!(layout.visible(853.0, 861.0), 1..1);
        assert_eq!(layout.visible(1000.0, 5000.0), 1..3);
        assert_eq!(layout.visible(5000.0, 6000.0), 3..3);
    }

    #[test]
    fn empty_document() {
        let layout = DocLayout::new(&[]);
        assert_eq!(layout.height(), 0.0);
        assert_eq!(layout.visible(0.0, 100.0), 0..0);
        assert_eq!(layout.page_at(50.0), 0);
        assert_eq!(layout.page_top(3), 0.0);
    }

    #[test]
    fn current_page_switches_in_the_middle_of_the_gap() {
        let layout = three_pages();
        assert_eq!(layout.page_at(0.0), 0);
        assert_eq!(layout.page_at(856.0), 0);
        assert_eq!(layout.page_at(858.0), 1);
        assert_eq!(layout.page_at(99_999.0), 2);
        assert_eq!(layout.page_top(1), 852.0);
    }

    #[test]
    fn thousands_of_pages_only_visit_the_visible_ones() {
        let pages = vec![size(595.0, 842.0); 5000];
        let layout = DocLayout::new(&pages);
        let range = layout.visible(2_000_000.0, 2_001_000.0);
        assert!(range.len() <= 3, "{range:?}");
    }
}
