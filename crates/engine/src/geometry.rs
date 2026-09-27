//! Page and tile geometry shared by the engine and the UI.
//!
//! Both sides must compute pixel sizes the same way, otherwise tiles would not line up.
//! All values here are in device pixels unless the name says `_pt` (PDF points, 1/72 inch).

/// Width and height of a square tile in device pixels.
pub const TILE_SIZE: u32 = 512;

/// Tile edge on slow pages. A 256-pixel tile renders in roughly a quarter of the time of a
/// 512-pixel one, so a slow area splits over several render processes; in total about 14%
/// more work, which is why ordinary pages keep [`TILE_SIZE`].
pub const SMALL_TILE_SIZE: u32 = 256;

/// Tile sizes the engine renders.
pub fn is_valid_tile_size(size: u32) -> bool {
    size == TILE_SIZE || size == SMALL_TILE_SIZE
}

/// Size of a page in PDF points, with the page's own `/Rotate` already applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageSize {
    pub width_pt: f32,
    pub height_pt: f32,
}

/// Render scale in device pixels per PDF point.
///
/// Stored as 16.16 fixed point so it can be used in hash keys: two requests with the same
/// `Scale` always produce the same pixel grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Scale(u32);

impl Scale {
    const ONE: f32 = 65536.0;

    /// The raw 16.16 fixed-point value, for sending a scale to a render worker.
    pub fn to_bits(self) -> u32 {
        self.0
    }

    pub fn from_bits(bits: u32) -> Self {
        Scale(bits)
    }

    pub fn from_px_per_pt(px_per_pt: f32) -> Self {
        Scale((px_per_pt * Self::ONE).round().max(1.0) as u32)
    }

    /// `zoom_percent` uses Acrobat's convention: 100% shows a page at its real size on a
    /// 96 DPI screen. `display_scale` is the Windows display scaling (1.0, 1.25, 1.5, ...).
    pub fn from_zoom(zoom_percent: f32, display_scale: f32) -> Self {
        Self::from_px_per_pt(zoom_percent / 100.0 * (96.0 / 72.0) * display_scale)
    }

    pub fn px_per_pt(self) -> f32 {
        self.0 as f32 / Self::ONE
    }
}

/// Full size of a page in device pixels at the given scale.
pub fn page_px_size(page: PageSize, scale: Scale) -> (u32, u32) {
    let s = scale.px_per_pt();
    (
        (page.width_pt * s).round().max(1.0) as u32,
        (page.height_pt * s).round().max(1.0) as u32,
    )
}

/// Number of tile columns and rows of `size` pixels needed to cover a page of the given
/// pixel size.
pub fn tile_grid(page_px: (u32, u32), size: u32) -> (u32, u32) {
    (page_px.0.div_ceil(size), page_px.1.div_ceil(size))
}

/// Area of the page covered by one tile. Tiles on the right and bottom edges are smaller
/// than the tile size when the page size is not a multiple of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Returns `None` if the tile lies outside the page.
pub fn tile_rect(page_px: (u32, u32), size: u32, tx: u32, ty: u32) -> Option<TileRect> {
    let x = tx.checked_mul(size)?;
    let y = ty.checked_mul(size)?;
    if size == 0 || x >= page_px.0 || y >= page_px.1 {
        return None;
    }
    Some(TileRect {
        x,
        y,
        width: (page_px.0 - x).min(size),
        height: (page_px.1 - y).min(size),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const A4: PageSize = PageSize {
        width_pt: 595.0,
        height_pt: 842.0,
    };

    #[test]
    fn zoom_100_matches_acrobat_at_96_dpi() {
        // An A4 page is 8.27 inches wide, which is 793 pixels at 96 DPI.
        let (w, h) = page_px_size(A4, Scale::from_zoom(100.0, 1.0));
        assert_eq!((w, h), (793, 1123));
    }

    #[test]
    fn display_scale_multiplies_pixels() {
        let (w, _) = page_px_size(A4, Scale::from_zoom(100.0, 1.5));
        assert_eq!(w, 1190);
    }

    #[test]
    fn grid_covers_page_and_edge_tiles_are_cropped() {
        let page_px = (1190, 1684);
        assert_eq!(tile_grid(page_px, TILE_SIZE), (3, 4));

        let last = tile_rect(page_px, TILE_SIZE, 2, 3).unwrap();
        assert_eq!(
            last,
            TileRect {
                x: 1024,
                y: 1536,
                width: 166,
                height: 148
            }
        );

        let covered: u64 = (0..3)
            .flat_map(|tx| (0..4).map(move |ty| (tx, ty)))
            .map(|(tx, ty)| tile_rect(page_px, TILE_SIZE, tx, ty).unwrap())
            .map(|r| r.width as u64 * r.height as u64)
            .sum();
        assert_eq!(covered, 1190 * 1684);
    }

    #[test]
    fn tiles_outside_the_page_are_rejected() {
        assert_eq!(tile_rect((1190, 1684), TILE_SIZE, 3, 0), None);
        assert_eq!(tile_rect((1190, 1684), TILE_SIZE, 0, 4), None);
        assert_eq!(tile_rect((1190, 1684), TILE_SIZE, u32::MAX, 0), None);
    }

    #[test]
    fn scale_is_stable_as_a_key() {
        assert_eq!(Scale::from_zoom(150.0, 1.25), Scale::from_zoom(150.0, 1.25));
        assert_ne!(Scale::from_zoom(150.0, 1.0), Scale::from_zoom(151.0, 1.0));
    }
}
